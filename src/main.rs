#![deny(
	absolute_paths_not_starting_with_crate,
	keyword_idents,
	macro_use_extern_crate,
	meta_variable_misuse,
	missing_abi,
	missing_copy_implementations,
	non_ascii_idents,
	nonstandard_style,
	noop_method_call,
	pointer_structural_match,
	private_in_public,
	rust_2018_idioms,
	unused_qualifications
)]
#![warn(clippy::pedantic)]
#![forbid(unsafe_code)]

use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::mpsc::RecvTimeoutError;

use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
	zwp_idle_inhibit_manager_v1, zwp_idle_inhibitor_v1,
};

macro_rules! proxies {
	(struct $name:ident { $($field:ident: $ty:path = $version:tt,)* }) => {
		#[derive(Default)]
		struct $name {
			$($field: Option<$ty>,)*
		}

		impl Dispatch<wl_registry::WlRegistry, ()> for  $name {
			fn event(
				state: &mut Self,
				registry: &wl_registry::WlRegistry,
				event: wl_registry::Event,
				_data: &(),
				_connection: &Connection,
				handle: &QueueHandle<Self>,
			) {
				if let wl_registry::Event::Global {
					name,
					interface,
					version: _,
				} = event
				{
					$({
						let wanted = <$ty as Proxy>::interface();
						if wanted.name == interface {
							state.$field = Some(registry.bind(name, $version, handle, ()));
							return;
						}
					})*
				}
			}
		}

		$(delegate_noop!(Proxies: ignore $ty);)*
	};
}

proxies! {
	struct Proxies {
		compositor: wl_compositor::WlCompositor = 1,
		idle_inhibit_manager: zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1 = 1,
	}
}

struct App {
	dummy_surface: wl_surface::WlSurface,
	manager: zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
	inhibitor: Option<zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1>,
	connection: Connection,
}

impl App {
	pub fn is_inhibited(&self) -> bool {
		self.inhibitor.is_some()
	}

	pub fn set_inhibited(&mut self, inhibited: bool) {
		if inhibited == self.is_inhibited() {
			return;
		}

		let mut queue = self.connection.new_event_queue();
		if inhibited {
			let inhibitor = self
				.manager
				.create_inhibitor(&self.dummy_surface, &queue.handle(), ());
			self.inhibitor = Some(inhibitor);
		} else {
			self.inhibitor.take().unwrap().destroy();
		}

		queue.roundtrip(&mut Ignored).unwrap();
	}
}

struct Ignored;

impl<T: Proxy> Dispatch<T, ()> for Ignored {
	fn event(
		_state: &mut Self,
		_proxy: &T,
		_event: <T as Proxy>::Event,
		_data: &(),
		_connection: &Connection,
		_handle: &QueueHandle<Self>,
	) {
	}
}

fn main() {
	let connection = Connection::connect_to_env().unwrap();
	let display = connection.display();

	let proxies = {
		let mut queue = connection.new_event_queue();
		let handle = queue.handle();

		let _registry = display.get_registry(&handle, ());

		let mut proxies = Proxies {
			compositor: None,
			idle_inhibit_manager: None,
		};
		queue.roundtrip(&mut proxies).unwrap();

		proxies
	};

	let idle_inhibit_manager = proxies
		.idle_inhibit_manager
		.expect("no idle inhibit manager");
	let compositor = proxies.compositor.expect("no compositor");

	let mut queue = connection.new_event_queue();
	let handle = queue.handle();
	let dummy_surface = compositor.create_surface(&handle, ());
	queue.roundtrip(&mut Ignored).unwrap();

	let mut app = App {
		dummy_surface,
		manager: idle_inhibit_manager,
		inhibitor: None,
		connection,
	};

	let (update_send, update_recv) = std::sync::mpsc::sync_channel(1);
	std::thread::spawn(move || 'initial: loop {
		let Ok(()) = update_recv.recv() else {
			break 'initial;
		};

		'debounced: loop {
			let res = update_recv.recv_timeout(std::time::Duration::from_secs(1));
			match res {
				Ok(()) => {}
				Err(RecvTimeoutError::Timeout) => break 'debounced,
				Err(RecvTimeoutError::Disconnected) => break 'initial,
			}
		}

		let any_running = check_uncorked();
		app.set_inhibited(any_running);
	});

	let audio_events = Command::new("pactl")
		.arg("subscribe")
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::inherit())
		.spawn()
		.unwrap();
	let audio_events = std::io::BufReader::new(audio_events.stdout.unwrap()).lines();

	update_send.send(()).unwrap();

	for event in audio_events {
		let event = event.unwrap();
		// Turns out that `pactl subscribe` sends events for when clients connect and disconnect from the bus. We only want change events on sink inputs and source outputs.
		let Some(on) = event.strip_prefix("Event 'change' on ") else {
			continue;
		};
		if !on.starts_with("sink-input") && !on.starts_with("source-output") {
			continue;
		}
		update_send.send(()).unwrap();
	}
}

fn check_uncorked() -> bool {
	let raw = Command::new("pactl").arg("list").output().unwrap().stdout;
	let raw = std::str::from_utf8(&raw).unwrap();
	raw.contains("Corked: no")
}

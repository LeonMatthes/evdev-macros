use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use evdev::{Device, InputEvent, InputEventKind, Key};
use notify_rust::Notification;
use signal_hook::consts::TERM_SIGNALS;
use std::{
    io,
    path::{Path, PathBuf},
    process::Child,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

fn process_events(device: &mut Device, sender: &mut Sender<InputEvent>) -> std::io::Result<()> {
    let events = device.fetch_events()?;
    for ev in events {
        sender.send(ev).ok();
    }
    Ok(())
}

fn grab_inputs(mut device: Device, mut sender: Sender<InputEvent>) {
    std::thread::spawn(move || {
        device.grab().unwrap();
        loop {
            if let Err(e) = process_events(&mut device, &mut sender) {
                eprintln!("Error: {}", e);
            }
        }
    });
}

struct MacroBoard {
    pub receiver: Receiver<InputEvent>,

    pub quit: bool,

    pub vendor: u16,
    pub product: u16,
}

impl MacroBoard {
    fn execute_script(&self, working_dir: &Path, path: &Path) -> io::Result<()> {
        eprintln!("Running macro: {path}", path = path.display());

        let old_euid = users::get_effective_uid();
        let old_egid = users::get_effective_gid();
        users::switch::set_effective_uid(users::get_current_uid())?;
        users::switch::set_effective_gid(users::get_current_gid())?;

        let result = Command::new(path)
            .stdin(Stdio::null())
            .current_dir(working_dir)
            .spawn();

        users::switch::set_effective_uid(old_euid).unwrap();
        users::switch::set_effective_gid(old_egid).unwrap();

        result.map(|mut child| {
            std::thread::spawn(move || {
                // We need to wait for our child process to finish,
                // Otherwise we're leaving defunct zombie processes behind.
                //
                // See: https://doc.rust-lang.org/std/process/struct.Child.html
                child.wait().ok();
            });
        })
    }

    fn run_macro(&self, macro_name: &str) -> Result<(), Box<dyn std::error::Error>> {
        let username = users::get_current_username()
            .map(|s| s.to_string_lossy().to_string())
            .ok_or("User no longer exists!")?;
        let config_path = PathBuf::from(format!("/home/{username}/.config/evdev-macros/"));
        let entries: Vec<_> = std::fs::read_dir(&config_path)?
            .filter_map(|entry| {
                if let Ok(entry) = entry {
                    if entry.path().file_stem().and_then(|s| s.to_str()) == Some(macro_name) {
                        return Some(entry);
                    }
                }
                None
            })
            .collect();
        for entry in entries {
            self.execute_script(&config_path, &entry.path())
                .and(Ok(()))?
        }

        Ok(())
    }

    fn process_event(&mut self, event: InputEvent) {
        if event.value() == 0 && event.kind() == InputEventKind::Key(Key::KEY_ESC) {
            eprintln!("Received ESC - exiting!");
            self.quit = true;
        }
        match (event.value(), event.kind()) {
            (0, InputEventKind::Key(key)) => {
                let key_name = format!("{key:?}");
                eprintln!("{key_name} - 0");
                if let Err(err) = self.run_macro(key_name.as_str()) {
                    eprintln!("Failed to execute macro: {err}");

                    let old_euid = users::get_effective_uid();
                    let old_egid = users::get_effective_gid();
                    users::switch::set_effective_uid(users::get_current_uid()).unwrap();
                    users::switch::set_effective_gid(users::get_current_gid()).unwrap();
                    Notification::new()
                        .summary(format!("Error executing {key_name} macro").as_str())
                        .body(err.to_string().as_str())
                        .show()
                        .ok();
                    users::switch::set_effective_uid(old_euid).ok();
                    users::switch::set_effective_gid(old_egid).ok();
                }
            }
            (value, InputEventKind::Key(key)) => eprintln!("{key:?} - {value}"),
            (_, _) => (),
        }
    }

    pub fn process_events(&mut self) {
        match self.receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => self.process_event(event),
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("Disconnected - exiting!");
                self.quit = true;
            }
            Err(RecvTimeoutError::Timeout) => (),
        }
    }
}

fn main() {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let mut board = MacroBoard {
        receiver,
        vendor: 0x413c,
        product: 0x2011,
        quit: false,
    };

    for (_path, device) in evdev::enumerate() {
        // println!("{}, {}", _path.to_string_lossy(), device);
        let ids = device.input_id();
        let supports_esc = device
            .supported_keys()
            .map(|keys| keys.contains(Key::KEY_ESC))
            .unwrap_or_default();
        if ids.vendor() == board.vendor && ids.product() == board.product && supports_esc {
            println!("Found keyboard:\n{device}");
            grab_inputs(device, sender.clone());
        }
    }
    drop(sender);

    let terminate = Arc::new(AtomicBool::new(false));
    for sig in TERM_SIGNALS {
        signal_hook::flag::register(*sig, Arc::clone(&terminate)).unwrap();
    }

    while !terminate.load(Ordering::Relaxed) && !board.quit {
        board.process_events();
    }
}

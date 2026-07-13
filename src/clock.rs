use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use chrono::{DateTime, Local, Timelike};
use gtk4::glib;

type Callback = Box<dyn Fn(DateTime<Local>) + 'static>;

pub struct Clock {
    second_subscribers: Vec<Callback>,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            second_subscribers: Vec::new(),
        }
    }

    pub fn on_second(mut self, callback: impl Fn(DateTime<Local>) + 'static) -> Self {
        self.second_subscribers.push(Box::new(callback));
        self
    }

    /// Start dispatching on the GTK main thread at wall-clock second boundaries.
    pub fn start(self) {
        let subscribers = Rc::new(self.second_subscribers);
        let generation = Rc::new(Cell::new(0_u64));

        let now = Local::now();
        for callback in subscribers.iter() {
            callback(now);
        }

        glib::timeout_add_local(Duration::from_millis(200), move || {
            let now = Local::now();
            let delay = Duration::from_millis(1_000 - u64::from(now.nanosecond() / 1_000_000));

            // Each guard tick supersedes its earlier one-shot. This keeps the
            // next update aligned after NTP corrections or manual clock jumps.
            let current_generation = generation.get().wrapping_add(1);
            generation.set(current_generation);

            let subscribers = subscribers.clone();
            let generation = generation.clone();
            glib::timeout_add_local_once(delay, move || {
                if generation.get() != current_generation {
                    return;
                }

                let now = Local::now();
                for callback in subscribers.iter() {
                    callback(now);
                }
            });

            glib::ControlFlow::Continue
        });
    }
}

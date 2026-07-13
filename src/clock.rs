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
        dispatch_and_schedule(subscribers);
    }
}

fn dispatch_and_schedule(subscribers: Rc<Vec<Callback>>) {
    let now = Local::now();
    for callback in subscribers.iter() {
        callback(now);
    }

    let delay = delay_until_next_second(now.nanosecond());
    glib::timeout_add_local_once(delay, move || dispatch_and_schedule(subscribers));
}

fn delay_until_next_second(nanosecond: u32) -> Duration {
    Duration::from_millis(1_000 - u64::from(nanosecond / 1_000_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_second_delay_is_bounded_and_aligned() {
        assert_eq!(delay_until_next_second(0), Duration::from_secs(1));
        assert_eq!(
            delay_until_next_second(500_000_000),
            Duration::from_millis(500)
        );
        assert_eq!(
            delay_until_next_second(999_999_999),
            Duration::from_millis(1)
        );
    }
}

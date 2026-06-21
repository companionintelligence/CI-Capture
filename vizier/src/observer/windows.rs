use crate::observer::common::{BaselineObserver, BaselineWaker};
use crate::observer::{Observer, ObserverConfig, WakeConfig, Waker};

pub fn create_observer(config: ObserverConfig) -> Box<dyn Observer> {
    Box::new(BaselineObserver::new(config))
}

pub fn create_waker(config: WakeConfig) -> Box<dyn Waker> {
    Box::new(BaselineWaker::new(config))
}

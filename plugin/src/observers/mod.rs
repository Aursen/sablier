pub mod state;
pub mod thread;

use std::{fmt::Debug, sync::Arc};

use thread::ThreadObserver;

#[derive(Default)]
pub struct Observers {
    pub thread: Arc<ThreadObserver>,
}

impl Observers {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Debug for Observers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "observers")
    }
}

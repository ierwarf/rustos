use spin::RwLock;

static TICK_JIFFIES_HOOK: RwLock<Option<fn(u64) -> u64>> = RwLock::new(None);
static INPUT_CONSUMER_ACQUIRE_HOOK: RwLock<Option<fn()>> = RwLock::new(None);
static INPUT_CONSUMER_RELEASE_HOOK: RwLock<Option<fn()>> = RwLock::new(None);

pub fn register_tick_jiffies_hook(hook: fn(u64) -> u64) {
    *TICK_JIFFIES_HOOK.write() = Some(hook);
}

pub fn tick_jiffies(delta: u64) -> u64 {
    if let Some(hook) = *TICK_JIFFIES_HOOK.read() {
        hook(delta)
    } else {
        0
    }
}

pub fn register_input_consumer_hooks(acquire: fn(), release: fn()) {
    *INPUT_CONSUMER_ACQUIRE_HOOK.write() = Some(acquire);
    *INPUT_CONSUMER_RELEASE_HOOK.write() = Some(release);
}

pub fn input_consumer_acquire() {
    if let Some(hook) = *INPUT_CONSUMER_ACQUIRE_HOOK.read() {
        hook();
    }
}

pub fn input_consumer_release() {
    if let Some(hook) = *INPUT_CONSUMER_RELEASE_HOOK.read() {
        hook();
    }
}

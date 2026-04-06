use crate::generated_registry;
use crate::user::runtime;

pub fn bootstrap() -> Result<(), runtime::DesktopRuntimeError> {
    runtime::bootstrap()?;
    let registered = generated_registry::register_desktop_programs()?;
    generated_registry::request_init_startup_programs(&registered)
}

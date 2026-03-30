use crate::generated_registry;
use crate::user::runtime;

pub fn bootstrap() -> Result<(), runtime::DesktopRuntimeError> {
    runtime::bootstrap()?;
    generated_registry::register_desktop_programs()
}

use crate::multitask;
use crate::user::console_host::ExecutableImage;
use crate::user::runtime;
use crate::user::runtime::{DesktopLaunchTarget, DesktopProgramRegistration};

const BOOT_PROGRAM: DesktopProgramRegistration = DesktopProgramRegistration::new(
    "UI Server",
    ExecutableImage::new("UISERVER.ELF"),
    multitask::DEFAULT_USER_TASK_WEIGHT_MICROS,
);
const BOOT_TARGET: DesktopLaunchTarget = DesktopLaunchTarget::FirstAvailableSession;

pub fn bootstrap() -> Result<(), runtime::DesktopRuntimeError> {
    runtime::bootstrap()?;

    let program_id = runtime::register_program(BOOT_PROGRAM)?;
    runtime::request_launch(program_id, BOOT_TARGET)?;

    Ok(())
}

use crate::multitask;
use crate::user::console_host::ExecutableImage;
use crate::user::runtime;
use crate::user::runtime::{DesktopLaunchTarget, DesktopProgramRegistration};

const UI_SERVER_PROGRAM: DesktopProgramRegistration = DesktopProgramRegistration::new(
    "UI Server",
    ExecutableImage::new("system/apps/uiserver/uiserver.elf"),
    multitask::DEFAULT_USER_TASK_WEIGHT_MICROS,
)
.with_logical_admin(true)
.with_console_hosted(false);
const PRINTF_CONSOLE_PROGRAM: DesktopProgramRegistration = DesktopProgramRegistration::new(
    "printf demo",
    ExecutableImage::new("system/apps/printfdemo/printfdemo.elf"),
    multitask::DEFAULT_USER_TASK_WEIGHT_MICROS,
);
pub fn bootstrap() -> Result<(), runtime::DesktopRuntimeError> {
    runtime::bootstrap()?;

    let ui_server_program_id = runtime::register_program(UI_SERVER_PROGRAM)?;
    runtime::request_launch(ui_server_program_id, DesktopLaunchTarget::NewSession)?;

    // The launcher can open multiple sessions from the same registered program.
    let printf_program_id = runtime::register_program(PRINTF_CONSOLE_PROGRAM)?;
    runtime::request_launch(printf_program_id, DesktopLaunchTarget::NewSession)?;

    let printf_program_id2 = runtime::register_program(PRINTF_CONSOLE_PROGRAM)?;
    runtime::request_launch(printf_program_id2, DesktopLaunchTarget::NewSession)?;

    Ok(())
}

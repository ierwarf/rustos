use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::driver;
use crate::io::session::ConsoleSessionHandle;
use crate::user::console_host::ExecutableImage;
use crate::user::runtime::{
    self, DesktopLaunchTarget, DesktopProgramRegistration, DesktopRuntimeError,
};

const DRIVER_REGISTRY_PATH: &str = "system/registry/kernel/loadable-drivers.tsv";
const DESKTOP_REGISTRY_PATH: &str = "system/registry/system/desktop-programs.tsv";

pub(crate) fn register_loadable_drivers() -> Result<usize, &'static str> {
    let contents = read_registry_text(DRIVER_REGISTRY_PATH)?;
    let mut registered = 0usize;

    for (line_number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let name = required_field(line, "name").map_err(|error| {
            leak_string(format_line_error(DRIVER_REGISTRY_PATH, line_number, error))
        })?;
        let class_name = required_field(line, "class").map_err(|error| {
            leak_string(format_line_error(DRIVER_REGISTRY_PATH, line_number, error))
        })?;
        let bus_name = required_field(line, "bus").map_err(|error| {
            leak_string(format_line_error(DRIVER_REGISTRY_PATH, line_number, error))
        })?;
        let priority = required_field(line, "priority").map_err(|error| {
            leak_string(format_line_error(DRIVER_REGISTRY_PATH, line_number, error))
        })?;
        let path = required_field(line, "path").map_err(|error| {
            leak_string(format_line_error(DRIVER_REGISTRY_PATH, line_number, error))
        })?;

        let class = driver::parse_driver_class(class_name).ok_or_else(|| {
            leak_string(format_line_error(
                DRIVER_REGISTRY_PATH,
                line_number,
                format!("unsupported driver class: {class_name}"),
            ))
        })?;
        let bus = driver::parse_driver_bus(bus_name).ok_or_else(|| {
            leak_string(format_line_error(
                DRIVER_REGISTRY_PATH,
                line_number,
                format!("unsupported driver bus: {bus_name}"),
            ))
        })?;
        let load_priority = priority.parse::<i32>().map_err(|_| {
            leak_string(format_line_error(
                DRIVER_REGISTRY_PATH,
                line_number,
                format!("invalid driver priority: {priority}"),
            ))
        })?;

        driver::register_loadable_elf_with_priority(
            leak_string(name.to_string()),
            class,
            bus,
            load_priority,
            leak_string(path.to_string()),
        );
        registered += 1;
    }

    Ok(registered)
}

pub(crate) fn register_desktop_programs() -> Result<(), DesktopRuntimeError> {
    let contents = read_registry_text(DESKTOP_REGISTRY_PATH).map_err(|error| {
        DesktopRuntimeError::Registry {
            path: DESKTOP_REGISTRY_PATH,
            error,
        }
    })?;

    let mut registrations = Vec::new();
    for (line_number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        registrations.push(parse_desktop_registration(line_number, line)?);
    }

    for (registration, launch) in registrations {
        let program_id = runtime::register_program(registration)?;
        match launch {
            DesktopLaunchTarget::Session(ConsoleSessionHandle::SYSTEM) => {}
            DesktopLaunchTarget::NewSession => {
                runtime::request_launch(program_id, DesktopLaunchTarget::NewSession)?;
            }
            DesktopLaunchTarget::AllSessions => {
                runtime::request_launch(program_id, DesktopLaunchTarget::AllSessions)?;
            }
            DesktopLaunchTarget::Session(session) => {
                runtime::request_launch(program_id, DesktopLaunchTarget::Session(session))?;
            }
        }
    }

    Ok(())
}

fn parse_desktop_registration(
    line_number: usize,
    line: &str,
) -> Result<(DesktopProgramRegistration, DesktopLaunchTarget), DesktopRuntimeError> {
    let display_name =
        required_field(line, "display_name").map_err(|error| DesktopRuntimeError::Registry {
            path: DESKTOP_REGISTRY_PATH,
            error: leak_string(format_line_error(DESKTOP_REGISTRY_PATH, line_number, error)),
        })?;
    let image_path =
        required_field(line, "image").map_err(|error| DesktopRuntimeError::Registry {
            path: DESKTOP_REGISTRY_PATH,
            error: leak_string(format_line_error(DESKTOP_REGISTRY_PATH, line_number, error)),
        })?;
    let exec_path =
        required_field(line, "exec").map_err(|error| DesktopRuntimeError::Registry {
            path: DESKTOP_REGISTRY_PATH,
            error: leak_string(format_line_error(DESKTOP_REGISTRY_PATH, line_number, error)),
        })?;
    let weight = required_field(line, "weight").map_err(|error| DesktopRuntimeError::Registry {
        path: DESKTOP_REGISTRY_PATH,
        error: leak_string(format_line_error(DESKTOP_REGISTRY_PATH, line_number, error)),
    })?;
    let logical_admin =
        required_field(line, "logical_admin").map_err(|error| DesktopRuntimeError::Registry {
            path: DESKTOP_REGISTRY_PATH,
            error: leak_string(format_line_error(DESKTOP_REGISTRY_PATH, line_number, error)),
        })?;
    let console_hosted =
        required_field(line, "console_hosted").map_err(|error| DesktopRuntimeError::Registry {
            path: DESKTOP_REGISTRY_PATH,
            error: leak_string(format_line_error(DESKTOP_REGISTRY_PATH, line_number, error)),
        })?;
    let launch = field(line, "launch").unwrap_or("none");
    let args = field(line, "args").unwrap_or_default();
    let env = field(line, "env").unwrap_or_default();

    let weight_micros = weight
        .parse::<u64>()
        .map_err(|_| DesktopRuntimeError::Registry {
            path: DESKTOP_REGISTRY_PATH,
            error: leak_string(format_line_error(
                DESKTOP_REGISTRY_PATH,
                line_number,
                format!("invalid desktop weight: {weight}"),
            )),
        })?;

    let logical_admin =
        parse_registry_bool(logical_admin).map_err(|error| DesktopRuntimeError::Registry {
            path: DESKTOP_REGISTRY_PATH,
            error: leak_string(format_line_error(DESKTOP_REGISTRY_PATH, line_number, error)),
        })?;
    let console_hosted =
        parse_registry_bool(console_hosted).map_err(|error| DesktopRuntimeError::Registry {
            path: DESKTOP_REGISTRY_PATH,
            error: leak_string(format_line_error(DESKTOP_REGISTRY_PATH, line_number, error)),
        })?;

    let image_path = leak_string(image_path.to_string());
    let exec_path = leak_string(exec_path.to_string());
    let display_name = leak_string(display_name.to_string());
    let args = leak_str_slice(args);
    let env = leak_str_slice(env);

    let mut registration = DesktopProgramRegistration::new(
        display_name,
        ExecutableImage::new(image_path),
        weight_micros,
    )
    .with_exec_path(exec_path)
    .with_logical_admin(logical_admin)
    .with_console_hosted(console_hosted);
    if !args.is_empty() {
        registration = registration.with_args(args);
    }
    if !env.is_empty() {
        registration = registration.with_env(env);
    }

    let launch = match launch {
        "none" | "" => DesktopLaunchTarget::Session(ConsoleSessionHandle::SYSTEM),
        "new-session" => DesktopLaunchTarget::NewSession,
        "all-sessions" => DesktopLaunchTarget::AllSessions,
        other => {
            return Err(DesktopRuntimeError::Registry {
                path: DESKTOP_REGISTRY_PATH,
                error: leak_string(format_line_error(
                    DESKTOP_REGISTRY_PATH,
                    line_number,
                    format!("unsupported launch mode: {other}"),
                )),
            });
        }
    };

    Ok((registration, launch))
}

fn read_registry_text(path: &'static str) -> Result<String, &'static str> {
    let bytes = crate::vfs::read_path_to_vec_for_kernel(path).map_err(|error| match error {
        crate::vfs::VfsError::NotFound => leak_string(format!("registry not found: {path}")),
        other => leak_string(format!("failed to read {path}: {:?}", other)),
    })?;
    String::from_utf8(bytes)
        .map_err(|_| leak_string(format!("registry is not valid UTF-8: {path}")))
}

fn required_field<'a>(line: &'a str, key: &str) -> Result<&'a str, String> {
    field(line, key).ok_or_else(|| format!("missing field: {key}"))
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    for token in line.split('\t') {
        let (candidate, value) = token.split_once('=')?;
        if candidate == key {
            return Some(value);
        }
    }
    None
}

fn parse_registry_bool(value: &str) -> Result<bool, String> {
    match value {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => Err(format!("invalid boolean value: {value}")),
    }
}

fn leak_string(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

fn leak_str_slice(value: &str) -> &'static [&'static str] {
    if value.is_empty() {
        return &[];
    }

    let mut parts = Vec::new();
    for item in value.split('|') {
        if item.is_empty() {
            continue;
        }
        parts.push(leak_string(item.to_string()));
    }
    Box::leak(parts.into_boxed_slice())
}

fn format_line_error(path: &str, line_number: usize, message: impl Into<String>) -> String {
    format!("{path}:{}: {}", line_number + 1, message.into())
}

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::driver;
use crate::user::console_host::{self, ExecutableImage};
use crate::user::runtime::{
    self, DesktopLaunchTarget, DesktopProgramRegistration, DesktopRuntimeError,
};

const DRIVER_REGISTRY_PATH: &str = "system/registry/kernel/loadable-drivers.tsv";
const DESKTOP_REGISTRY_PATH: &str = "system/registry/system/desktop-programs.tsv";
const STARTUP_REGISTRY_PATH: &str = "system/registry/system/startup-programs.tsv";
const INITD_EXEC_PATH: &str = "services/initd/initd.elf";

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

pub(crate) fn register_desktop_programs()
-> Result<Vec<runtime::DesktopProgramInfo>, DesktopRuntimeError> {
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

    let mut registered = Vec::with_capacity(registrations.len());
    for registration in registrations {
        if should_prime_desktop_image(&registration)
            && let Err(err) = console_host::prime_executable_image(registration.image)
        {
            err.log_debug_details();
        }
        let program_id = runtime::register_program(registration)?;
        let info = runtime::snapshot_program_info(program_id)
            .ok_or(DesktopRuntimeError::ProgramNotFound { program_id })?;
        registered.push(info);
    }

    Ok(registered)
}

fn should_prime_desktop_image(registration: &DesktopProgramRegistration) -> bool {
    let _ = registration;
    false
}

fn parse_desktop_registration(
    line_number: usize,
    line: &str,
) -> Result<DesktopProgramRegistration, DesktopRuntimeError> {
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

    Ok(registration)
}

pub(crate) fn request_init_startup_programs(
    registered: &[runtime::DesktopProgramInfo],
) -> Result<(), DesktopRuntimeError> {
    let contents = match read_registry_text(STARTUP_REGISTRY_PATH) {
        Ok(contents) => contents,
        Err(_) => return Ok(()),
    };
    let mut launched = false;

    for (line_number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mode = required_field(line, "mode").map_err(|error| DesktopRuntimeError::Registry {
            path: STARTUP_REGISTRY_PATH,
            error: leak_string(format_line_error(STARTUP_REGISTRY_PATH, line_number, error)),
        })?;
        if mode != "init" {
            continue;
        }

        let exec = required_field(line, "exec").map_err(|error| DesktopRuntimeError::Registry {
            path: STARTUP_REGISTRY_PATH,
            error: leak_string(format_line_error(STARTUP_REGISTRY_PATH, line_number, error)),
        })?;
        if exec != INITD_EXEC_PATH {
            continue;
        }
        let display_name = field(line, "display_name");
        let Some(program_id) = find_registered_program(registered, exec, display_name) else {
            return Err(DesktopRuntimeError::Registry {
                path: STARTUP_REGISTRY_PATH,
                error: leak_string(format!(
                    "startup entry did not match a registered program: exec={exec}"
                )),
            });
        };
        crate::debug::println!(
            "startup registry launch: mode={} exec={} program_id={}",
            mode,
            exec,
            program_id.index(),
        );
        if launched {
            break;
        }
        runtime::request_launch(program_id, DesktopLaunchTarget::NewSession)?;
        launched = true;
    }

    if !launched {
        return Err(DesktopRuntimeError::Registry {
            path: STARTUP_REGISTRY_PATH,
            error: leak_string(format!(
                "missing bootstrap init service: exec={INITD_EXEC_PATH}"
            )),
        });
    }

    Ok(())
}

fn find_registered_program(
    registered: &[runtime::DesktopProgramInfo],
    exec: &str,
    name: Option<&str>,
) -> Option<runtime::DesktopProgramId> {
    if let Some(program) = registered.iter().find(|program| program.exec_path == exec) {
        return Some(program.id);
    }
    if let Some(name) = name {
        if let Some(program) = registered
            .iter()
            .find(|program| program.display_name == name)
        {
            return Some(program.id);
        }
    }
    None
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct AutostartEntry {
    name: Option<String>,
    exec: String,
}

#[cfg(test)]
fn parse_autostart_entry(
    contents: &str,
    path: &str,
) -> Result<Option<AutostartEntry>, &'static str> {
    let mut in_desktop_entry = false;
    let mut entry_type = None::<&str>;
    let mut hidden = false;
    let mut no_display = false;
    let mut enabled = true;
    let mut only_show_in = None::<&str>;
    let mut not_show_in = None::<&str>;
    let mut name = None::<String>;
    let mut exec = None::<String>;

    for (line_number, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Type" => entry_type = Some(value),
            "Name" => name = Some(value.to_string()),
            "Exec" => {
                exec = parse_desktop_exec_target(value).map(str::to_string);
            }
            "Hidden" => hidden = parse_desktop_bool(value),
            "NoDisplay" => no_display = parse_desktop_bool(value),
            "X-GNOME-Autostart-enabled" => enabled = parse_desktop_bool(value),
            "OnlyShowIn" => only_show_in = Some(value),
            "NotShowIn" => not_show_in = Some(value),
            _ => {}
        }

        if key.trim() == "Exec" && exec.is_none() {
            return Err(leak_string(format_line_error(
                path,
                line_number,
                "invalid Exec field in autostart entry",
            )));
        }
    }

    if !matches!(entry_type, None | Some("Application")) || hidden || no_display || !enabled {
        return Ok(None);
    }
    if let Some(only_show_in) = only_show_in {
        if !desktop_list_contains(only_show_in, "RustOS") {
            return Ok(None);
        }
    }
    if let Some(not_show_in) = not_show_in {
        if desktop_list_contains(not_show_in, "RustOS") {
            return Ok(None);
        }
    }

    let Some(exec) = exec else {
        return Ok(None);
    };

    Ok(Some(AutostartEntry { name, exec }))
}

#[cfg(test)]
fn parse_desktop_exec_target(value: &str) -> Option<&str> {
    let value = value.trim_start();
    if value.is_empty() {
        return None;
    }

    let bytes = value.as_bytes();
    if matches!(bytes.first(), Some(b'"') | Some(b'\'')) {
        let quote = bytes[0];
        let end = bytes[1..]
            .iter()
            .position(|candidate| *candidate == quote)
            .map(|index| index + 1)?;
        return Some(&value[1..end]);
    }

    let end = value.find(char::is_whitespace).unwrap_or(value.len());
    Some(&value[..end])
}

#[cfg(test)]
fn parse_desktop_bool(value: &str) -> bool {
    matches!(value, "1" | "true" | "True" | "yes" | "Yes")
}

#[cfg(test)]
fn desktop_list_contains(value: &str, entry: &str) -> bool {
    value
        .split(';')
        .map(str::trim)
        .any(|candidate| !candidate.is_empty() && candidate == entry)
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

#[cfg(test)]
mod tests {
    use super::{desktop_list_contains, parse_autostart_entry, parse_desktop_exec_target};

    #[test]
    fn parse_desktop_exec_target_extracts_command_path() {
        assert_eq!(
            parse_desktop_exec_target("apps/wayclick/wayclick.elf --fullscreen"),
            Some("apps/wayclick/wayclick.elf")
        );
        assert_eq!(
            parse_desktop_exec_target("\"services/uiserver/uiserver.elf\""),
            Some("services/uiserver/uiserver.elf")
        );
    }

    #[test]
    fn parse_autostart_entry_reads_desktop_entry() {
        let entry = parse_autostart_entry(
            "[Desktop Entry]\nType=Application\nName=WayClick\nExec=apps/wayclick/wayclick.elf\nOnlyShowIn=RustOS;\n",
            "/etc/xdg/autostart/wayclick.desktop",
        )
        .expect("parse succeeds")
        .expect("entry enabled");
        assert_eq!(entry.name.as_deref(), Some("WayClick"));
        assert_eq!(entry.exec, "apps/wayclick/wayclick.elf");
    }

    #[test]
    fn parse_autostart_entry_skips_non_matching_desktop() {
        let entry = parse_autostart_entry(
            "[Desktop Entry]\nType=Application\nExec=apps/wayclick/wayclick.elf\nOnlyShowIn=GNOME;\n",
            "/etc/xdg/autostart/wayclick.desktop",
        )
        .expect("parse succeeds");
        assert!(entry.is_none());
        assert!(desktop_list_contains("RustOS;GNOME;", "RustOS"));
    }
}

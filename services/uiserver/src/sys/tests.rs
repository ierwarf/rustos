use super::{
    EV_KEY, INPUT_ACTION_PRESSED, INPUT_ACTION_RELEASED, INPUT_KIND_KEYBOARD,
    InputTranslationState, LinuxInputEvent, LinuxInputTimeval, POLLERR, POLLHUP, POLLIN,
    POLLNVAL, UiThreadRole, input_poll_revents, translate_linux_input_event, trusted_ui_status,
};
use rustos_user_abi::{device, syscall};

fn key_event(code: u16, value: i32) -> LinuxInputEvent {
    LinuxInputEvent {
        time: LinuxInputTimeval::default(),
        kind: EV_KEY,
        code,
        value,
    }
}

#[test]
fn linux_key_press_emits_text_before_release() {
    let mut state = InputTranslationState::default();

    let press = translate_linux_input_event(&mut state, key_event(30, 1))
        .expect("A press should translate");
    assert_eq!(press.kind, INPUT_KIND_KEYBOARD);
    assert_eq!(press.action, INPUT_ACTION_PRESSED);
    assert_eq!(press.text, b'a' as u32);

    let release = translate_linux_input_event(&mut state, key_event(30, 0))
        .expect("A release should translate");
    assert_eq!(release.kind, INPUT_KIND_KEYBOARD);
    assert_eq!(release.action, INPUT_ACTION_RELEASED);
    assert_eq!(release.text, 0);
}

#[test]
fn trusted_ui_status_fails_closed_for_every_current_scanout() {
    let baseline = syscall::UISERVER_TRUSTED_UI_STATUS_UNATTESTED_SCANOUT
        | syscall::UISERVER_TRUSTED_UI_STATUS_UNATTESTED_INPUT;
    assert_eq!(trusted_ui_status(0), baseline);
    assert_eq!(
        trusted_ui_status(device::DISPLAY_INFO_FLAG_PRIMARY_PROVIDER),
        baseline
    );
    assert_eq!(
        trusted_ui_status(
            device::DISPLAY_INFO_FLAG_PRIMARY_PROVIDER | device::DISPLAY_INFO_FLAG_DVM_SCANOUT
        ),
        baseline | syscall::UISERVER_TRUSTED_UI_STATUS_DVM_SCANOUT
    );
}

#[test]
fn input_poll_requires_readable_without_hiding_terminal_status() {
    assert_eq!(input_poll_revents(0), Ok(false));
    assert_eq!(input_poll_revents(POLLIN), Ok(true));
    for terminal in [POLLERR, POLLHUP, POLLNVAL] {
        assert_eq!(input_poll_revents(terminal), Err(5));
        assert_eq!(input_poll_revents(POLLIN | terminal), Err(5));
    }
}

#[test]
fn only_bounded_bootstrap_roles_retain_inherited_boot_class() {
    for role in [
        UiThreadRole::Background,
        UiThreadRole::Protocol,
        UiThreadRole::DisplayPolicy,
        UiThreadRole::Watchdog,
    ] {
        assert!(!role.retains_boot_class(), "unexpected boot role {role:?}");
    }
    assert!(UiThreadRole::BootstrapGpu.retains_boot_class());
    assert!(UiThreadRole::BootstrapDesktop.retains_boot_class());
}

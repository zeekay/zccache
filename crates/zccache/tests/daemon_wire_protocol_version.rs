use zccache::protocol::wire_prost::{wire_format_from_env_value, WireFormat, WIRE_FORMAT_ENV};
use zccache::protocol::{BINCODE_PROTOCOL_VERSION, PROST_PROTOCOL_VERSION, PROTOCOL_VERSION};

#[test]
fn current_protocol_version_remains_v15_bincode() {
    assert_eq!(PROTOCOL_VERSION, BINCODE_PROTOCOL_VERSION);
    assert_ne!(PROTOCOL_VERSION, PROST_PROTOCOL_VERSION);
}

#[test]
fn wire_env_accepts_current_and_planned_modes() {
    assert_eq!(
        wire_format_from_env_value(None).unwrap(),
        WireFormat::ProstV16
    );
    assert_eq!(
        wire_format_from_env_value(Some("auto")).unwrap(),
        WireFormat::ProstV16
    );
    assert_eq!(
        wire_format_from_env_value(Some("bincode")).unwrap(),
        WireFormat::BincodeV15
    );
    assert_eq!(
        wire_format_from_env_value(Some("prost")).unwrap(),
        WireFormat::ProstV16
    );
    assert_eq!(
        wire_format_from_env_value(Some("v16")).unwrap(),
        WireFormat::ProstV16
    );
}

#[test]
fn wire_env_rejects_unknown_values() {
    let err = wire_format_from_env_value(Some("json")).unwrap_err();
    assert!(err.contains(WIRE_FORMAT_ENV));
    assert!(err.contains("bincode"));
    assert!(err.contains("prost"));
}

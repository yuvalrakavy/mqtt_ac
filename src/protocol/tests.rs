use serde_json::json;

use super::*;

fn map(value: Value) -> PropertyMap {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn commands(unit: &str, values: Value) -> Vec<Result<String, OpError>> {
    plan(unit, &map(values)).expect("a unit address").into_iter().map(|step| step.command).collect()
}

fn ok(lines: &[&str]) -> Vec<Result<String, OpError>> {
    lines.iter().map(|line| Ok((*line).to_owned())).collect()
}

/// A reply's status decides it: `OK` (and `ERROR: 0`, as v1 had it) is success, with the body;
/// any other text is the CoolMaster refusing the command, as v1 took it — `ERROR: n`, or a
/// firmware's words (`Unknown command`, `Unsupported Feature`) — `Rejected`, which keeps the
/// link up.
#[test]
fn an_error_status_is_a_rejection_and_ok_is_success() {
    assert_eq!(parse_reply(b"OK\r\n"), Ok(String::new()));
    assert_eq!(parse_reply(b"ERROR: 0\r\n"), Ok(String::new()));
    assert_eq!(parse_reply(b"L1.001 ON 22.0C 25.0C High Cool OK - 0\r\nOK\r\n"), Ok("L1.001 ON 22.0C 25.0C High Cool OK - 0".to_owned()));
    assert_eq!(parse_reply(b"a\r\nb\r\nOK\r\n"), Ok("a\r\nb".to_owned()));
    for refused in
        [&b"ERROR: 1\r\n"[..], b"ERROR: 3", b"body\r\nERROR: 2\r\n", b"Unknown command\r\n", b"Unsupported Feature\r\n", b"a\r\nb"]
    {
        assert!(
            matches!(parse_reply(refused), Err(OpError::Rejected(_))),
            "{:?} was not a rejection: {:?}",
            String::from_utf8_lossy(refused),
            parse_reply(refused)
        );
    }
}

/// A reply that cannot be read — not text, or empty — is `Unusable`: reported and passed over,
/// never a reason to reconnect, and never taken for the CoolMaster's refusal.
#[test]
fn a_reply_that_cannot_be_read_is_unusable() {
    for unusable in [&b"\xff\xfe\r\nOK\r\n"[..], b"", b"\r\n", b"  "] {
        assert!(
            matches!(parse_reply(unusable), Err(OpError::Unusable(_))),
            "{:?} was not unusable: {:?}",
            String::from_utf8_lossy(unusable),
            parse_reply(unusable)
        );
    }
}

/// A unit's `ls2` line becomes its State document, with v1's field names and value strings.
#[test]
fn an_ls2_line_is_a_state_document() {
    let state = parse_line("L4.001 OFF 19.0C 23.5C Med Heat OK   - 0").unwrap();
    assert_eq!(state.target, "L4.001");
    assert_eq!(
        Value::Object(state.values),
        json!({
            "unit": "L4.001", "power": false, "target_temperature": 19.0, "temperature": 23.5,
            "fan_speed": "Medium", "operation_mode": "Heat", "failure_code": null,
            "filter_change": false, "demand": false
        })
    );
    let state = parse_line("L7.400 ON 77F 72.5F VLow Fan 12 # 1").unwrap();
    assert_eq!(state.values["target_temperature"], json!(25.0));
    assert_eq!(state.values["temperature"], json!(22.5));
    assert_eq!(state.values["fan_speed"], json!("VLow"));
    assert_eq!(state.values["failure_code"], json!("12"));
    assert_eq!(state.values["filter_change"], json!(true));
    assert_eq!(state.values["demand"], json!(true));
    for (word, value) in [("Low", "Low"), ("High", "High"), ("Top", "Top"), ("Auto", "Auto")] {
        let state = parse_line(&format!("L1.001 ON 22C 22C {word} Auto OK - 0")).unwrap();
        assert_eq!(state.values["fan_speed"], json!(value));
    }
    for mode in ["Cool", "Heat", "Dry", "Fan", "Auto"] {
        let state = parse_line(&format!("L1.001 ON 22C 22C Low {mode} OK - 0")).unwrap();
        assert_eq!(state.values["operation_mode"], json!(mode));
    }
}

/// A unit in failure keeps its State: the failure code is the CoolMaster's own text — vendor codes
/// are often alphanumeric (`A3`, `U4`) — and `null` for `OK`. As a number only (v1), an
/// alphanumeric code made the whole line unusable, and the unit's State froze for as long as it
/// was in failure, just when it mattered.
#[test]
fn a_failure_code_is_the_controllers_text() {
    for code in ["A3", "U4", "E-01", "12", "6602"] {
        let line = format!("L1.001 ON 22.0C 25.0C High Cool {code} - 0");
        let parsed = parse_line(&line);
        assert!(
            parsed.as_ref().is_ok_and(|s| s.values["failure_code"] == json!(code)),
            "a unit in failure with code `{code}` was not read as that code: {parsed:?}"
        );
    }
    assert_eq!(parse_line("L1.001 ON 22.0C 25.0C High Cool OK - 0").unwrap().values["failure_code"], Value::Null);
}

/// A line that does not parse says why, and never panics — a multibyte temperature included
/// (v1's re-review C5: a byte slice inside `°` ended the worker).
#[test]
fn a_line_that_does_not_parse_says_why() {
    for bad in [
        "",
        "L1.001 ON 22.0C 25.0C High Cool OK -",
        "L1.001 ON 22.0C 25.0C High Cool OK - 0 extra",
        "L1.001 MAYBE 22.0C 25.0C High Cool OK - 0",
        "L1.001 ON 25° 23.0C High Cool OK - 0",
        "L1.001 ON 22.0K 25.0C High Cool OK - 0",
        "L1.001 ON NaNC 25.0C High Cool OK - 0",
        "L1.001 ON infC 25.0C High Cool OK - 0",
        "L1.001 ON 1e308F 25.0C High Cool OK - 0",
        "L1.001 ON 22.0C -1e308F High Cool OK - 0",
        "L1.001 ON 22.0C 25.0C Turbo Cool OK - 0",
        "L1.001 ON 22.0C 25.0C High Freeze OK - 0",
        "L1.001 ON 22.0C 25.0C High Cool OK x 0",
        "L1.001 ON 22.0C 25.0C High Cool OK - 2",
        "L1/001 ON 22.0C 25.0C High Cool OK - 0",
    ] {
        let parsed = std::panic::catch_unwind(|| parse_line(bad));
        assert!(matches!(parsed, Ok(Err(_))), "`{bad}` was not refused: {parsed:?}");
    }
}

/// A listing is read a line at a time: one bad line costs only its own unit, and says which and
/// why (v1 failed the whole listing on it, so no unit's State changed).
#[test]
fn one_bad_line_costs_only_its_own_unit() {
    let body = "L1.001 ON 22.0C 25.0C High Cool OK - 0\r\nL1.002 ON 24.0C ??? Low Heat OK # 1\r\n\r\nL1.003 OFF 20.0C 21.0C Auto Dry 7 - 0";
    let listing = parse_listing(body);
    let units: Vec<&str> = listing.states.iter().map(|s| s.target.as_str()).collect();
    assert_eq!(units, ["L1.001", "L1.003"]);
    assert_eq!(listing.bad.len(), 1, "{:?}", listing.bad);
    assert_eq!((listing.bad[0].unit.as_str(), listing.bad[0].line.as_str()), ("L1.002", "L1.002 ON 24.0C ??? Low Heat OK # 1"));
    assert!(listing.bad[0].why.contains("???"), "{:?}", listing.bad[0]);
    assert_eq!(listing.into_states().map(|s| s.len()), Ok(2));
}

/// A listing with no usable line is `Unusable`; an empty one is no units.
#[test]
fn a_listing_with_no_usable_line_is_unusable() {
    assert!(matches!(parse_listing("garbage\r\nL1.001 ON").into_states(), Err(OpError::Unusable(_))));
    assert_eq!(parse_listing("").into_states(), Ok(Vec::new()));
    assert_eq!(parse_listing("\r\n").into_states(), Ok(Vec::new()));
}

/// The commands, formatted as v1 sent them.
#[test]
fn each_property_is_its_command() {
    assert_eq!(commands("L7.400", json!({"power": true})), ok(&["on L7.400"]));
    assert_eq!(commands("L7.400", json!({"power": false})), ok(&["off L7.400"]));
    for (mode, word) in [("Cool", "cool"), ("Heat", "heat"), ("Dry", "dry"), ("Fan", "fan"), ("Auto", "auto")] {
        assert_eq!(commands("L7.400", json!({"operation_mode": mode})), ok(&[&format!("{word} L7.400")]));
    }
    for (speed, letter) in [("VLow", "v"), ("Low", "l"), ("Medium", "m"), ("High", "h"), ("Top", "t"), ("Auto", "a")] {
        assert_eq!(commands("L7.400", json!({"fan_speed": speed})), ok(&[&format!("fspeed L7.400 {letter}")]));
    }
    assert_eq!(commands("L7.400", json!({"target_temperature": 22})), ok(&["temp L7.400 22.0"]));
    assert_eq!(commands("L7.400", json!({"target_temperature": 22.0})), ok(&["temp L7.400 22.0"]));
    assert_eq!(commands("L7.400", json!({"target_temperature": 21.5})), ok(&["temp L7.400 21.5"]));
    assert_eq!(list(None), Ok("ls2".to_owned()));
    assert_eq!(list(Some("L1.001")), Ok("ls2 L1.001".to_owned()));
    assert_eq!(reset_filter("L1.001"), Ok("filt L1.001".to_owned()));
}

/// A setpoint goes to the CoolMaster at its step, 0.1 °C, with one decimal, as v1 sent it
/// (`21.7`): never the noise of the Store's arithmetic (`21.700000000000003`). One outside
/// 0–50 °C, or not finite, is `Rejected` unsent.
#[test]
fn a_setpoint_is_rounded_to_the_coolmasters_step_and_kept_in_range() {
    for (value, sent) in [
        (json!(21.700000000000003), "21.7"),
        (json!(0.1 + 0.2 + 21.4), "21.7"),
        (json!(22.04), "22.0"),
        (json!(22.06), "22.1"),
        (json!(0), "0.0"),
        (json!(50), "50.0"),
    ] {
        assert_eq!(
            commands("L1.001", json!({ "target_temperature": value })),
            ok(&[&format!("temp L1.001 {sent}")]),
            "the setpoint {value} was not sent as {sent}"
        );
    }
    for value in [json!(1e300), json!(-0.5), json!(50.1), json!(-1e300)] {
        let sent = commands("L1.001", json!({ "target_temperature": value }));
        assert!(matches!(sent[0], Err(OpError::Rejected(_))), "the setpoint {value} was not refused: {sent:?}");
    }
}

/// A unit's scale is the one its setpoint is listed in; a setpoint asked in °C goes to a unit in
/// °F in °F, at the same step (23 °C as `73.4`, 21.7 °C as `71.1`), still checked in °C.
#[test]
fn a_setpoint_goes_in_the_units_own_scale() {
    let listing = parse_listing("L1.001 ON 72.0F 77.0F High Cool OK - 0\r\nL1.002 ON 22.0C 25.0C High Cool OK - 0");
    assert_eq!(listing.scales.get("L1.001"), Some(&Scale::Fahrenheit));
    assert_eq!(listing.scales.get("L1.002"), Some(&Scale::Celsius));
    let sent = |value: Value| {
        plan_scaled("L1.001", &map(json!({ "target_temperature": value })), Some(Scale::Fahrenheit)).unwrap()[0].command.clone()
    };
    assert_eq!(sent(json!(23)), Ok("temp L1.001 73.4".to_owned()), "a °C setpoint was not sent to a °F unit in °F");
    assert_eq!(sent(json!(21.7)), Ok("temp L1.001 71.1".to_owned()));
    assert!(matches!(sent(json!(60)), Err(OpError::Rejected(_))), "a setpoint out of range in °C was sent in °F");
    assert_eq!(commands("L1.001", json!({"target_temperature": 23})), ok(&["temp L1.001 23.0"]));
    // The scale unknown: the setpoint refused, never sent on a guess; the rest still goes.
    let steps = plan_scaled("L1.001", &map(json!({"target_temperature": 23, "power": true})), None).unwrap();
    assert_eq!(steps[0].command, Ok("on L1.001".to_owned()));
    assert!(
        matches!(&steps[1].command, Err(OpError::Rejected(e)) if e.contains("unknown")),
        "a setpoint was planned on a guess: {steps:?}"
    );
}

/// An `apply`'s commands go in the order the CoolMaster needs: power on first; the mode before the
/// setpoint and the fan speed; power off last.
#[test]
fn an_apply_goes_power_on_first_then_mode_setpoint_fan_and_power_off_last() {
    let all = |power: bool| json!({"fan_speed": "Low", "target_temperature": 24, "power": power, "operation_mode": "Heat"});
    assert_eq!(
        commands("L1.001", all(true)),
        ok(&["on L1.001", "heat L1.001", "temp L1.001 24.0", "fspeed L1.001 l"]),
        "turning a unit on does not go power first, then mode, setpoint, fan"
    );
    assert_eq!(
        commands("L1.001", all(false)),
        ok(&["heat L1.001", "temp L1.001 24.0", "fspeed L1.001 l", "off L1.001"]),
        "turning a unit off does not go mode, setpoint, fan, then power last"
    );
    assert_eq!(
        commands("L1.001", json!({"fan_speed": "Top", "operation_mode": "Cool"})),
        ok(&["cool L1.001", "fspeed L1.001 t"]),
        "the mode does not go before the fan speed"
    );
}

/// A value outside its property's vocabulary is `Rejected`, a property no request can set is
/// `Unsupported` — each in its own step, so the rest of the request still applies.
#[test]
fn a_value_outside_its_vocabulary_is_rejected_and_an_unknown_property_unsupported() {
    let steps = plan("L1.001", &map(json!({"power": false, "fan_speed": "Turbo", "swing": true, "target_temperature": "22"}))).unwrap();
    let by_property = |p: &str| steps.iter().find(|s| s.property == p).map(|s| s.command.clone()).unwrap();
    assert!(matches!(by_property("fan_speed"), Err(OpError::Rejected(_))), "{steps:?}");
    assert!(matches!(by_property("target_temperature"), Err(OpError::Rejected(_))), "{steps:?}");
    assert!(matches!(by_property("swing"), Err(OpError::Unsupported(_))), "{steps:?}");
    assert_eq!(by_property("power"), Ok("off L1.001".to_owned()));
    assert_eq!(steps.last().map(|s| s.property.as_str()), Some("power"), "power off is not last: {steps:?}");
    for bad in [json!({"power": "true"}), json!({"power": 1}), json!({"operation_mode": "cool"}), json!({"fan_speed": "Med"})] {
        let steps = plan("L1.001", &map(bad.clone())).unwrap();
        assert!(matches!(steps[0].command, Err(OpError::Rejected(_))), "{bad} was not rejected: {steps:?}");
    }
}

/// A target that is not a unit address is refused before anything is sent: one with a space or a
/// `\r` would end the command early and start another on the CoolMaster, and an empty one would
/// make `filt` reset every unit's filter.
#[test]
fn a_target_that_is_not_a_unit_address_is_rejected() {
    for bad in ["", "L1.001 L1.002", "L1.001\roff L1.002", "L1.001\n", "L1;001", "*"] {
        assert!(matches!(plan(bad, &map(json!({"power": true}))), Err(OpError::Rejected(_))), "{bad:?} was planned");
        assert!(matches!(reset_filter(bad), Err(OpError::Rejected(_))), "{bad:?} was reset");
        assert!(matches!(list(Some(bad)), Err(OpError::Rejected(_))), "{bad:?} was listed");
    }
    for good in ["L7.400", "L1.001", "100", "M1-2_3"] {
        assert!(valid_unit(good), "{good}");
    }
}

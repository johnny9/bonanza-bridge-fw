use heapless::Vec;

pub const SAFETY_STATUS_SCHEMA_VERSION: u8 = 2;
pub const SAFETY_STATUS_ENCODED_LEN: usize = 13;
/// Policy identifier 3 retains the 1002 trip latch without a host lease.
/// Identifiers 0..=2 belonged to the protocol-1 policies.
pub const SAFETY_POLICY_TRIP_LATCH_NO_LEASE: u8 = 3;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafetyWireCommand {
    GetStatus = 0x10,
    ClearFault = 0x13,
    SafeOff = 0x14,
}

pub fn decode_wire_command(bytes: &[u8]) -> Option<SafetyWireCommand> {
    match bytes {
        [0x10] => Some(SafetyWireCommand::GetStatus),
        // Former ARM and HEARTBEAT opcodes stay reserved, not successful no-ops.
        [0x13] => Some(SafetyWireCommand::ClearFault),
        [0x14] => Some(SafetyWireCommand::SafeOff),
        _ => None,
    }
}

pub const CAP_FIVE_VOLT_CONTROL: u16 = 1 << 0;
pub const CAP_ASIC_RESET_CONTROL: u16 = 1 << 1;
pub const CAP_FAN_FORCE_FULL: u16 = 1 << 2;
pub const CAP_TRIP_INPUT_SAMPLED: u16 = 1 << 3;
pub const CAP_CORE_POWER_CUTOFF: u16 = 1 << 4;
pub const CAP_FAN_TACH_INTERLOCK: u16 = 1 << 5;
pub const CAP_INDEPENDENT_TRIP_MONITOR: u16 = 1 << 6;
pub const CAP_FAN_CONTROLLED_SPEED: u16 = 1 << 7;

pub const EVIDENCE_OUTPUTS_SAFE: u16 = 1 << 0;
// Evidence bit 1 was LEASE_VALID in schema 1; it remains reserved and clear.
pub const EVIDENCE_TRIP_CLEAR: u16 = 1 << 2;
pub const EVIDENCE_FAULT_CLEAR: u16 = 1 << 3;
pub const EVIDENCE_CORE_CUTOFF_AVAILABLE: u16 = 1 << 4;
pub const EVIDENCE_FAN_TACH_INTERLOCK_AVAILABLE: u16 = 1 << 5;
pub const EVIDENCE_INDEPENDENT_TRIP_MONITOR_AVAILABLE: u16 = 1 << 6;

const REQUIRED_PRODUCTION_CAPABILITIES: u16 = CAP_FIVE_VOLT_CONTROL | CAP_ASIC_RESET_CONTROL | CAP_FAN_FORCE_FULL | CAP_TRIP_INPUT_SAMPLED | CAP_CORE_POWER_CUTOFF | CAP_FAN_TACH_INTERLOCK | CAP_INDEPENDENT_TRIP_MONITOR | CAP_FAN_CONTROLLED_SPEED;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafetyState {
    SafeOff = 0,
    Controlled = 1,
    FaultLatched = 2,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultReason {
    None = 0,
    // Value 1 is reserved for the removed LEASE_EXPIRED fault.
    AsicTrip = 2,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeVerdict {
    GoodSafeOff = 0,
    GoodControlled = 1,
    BadFault = 0x80,
    BadTripInput = 0x82,
    BadUnsafeOutputs = 0x83,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductionVerdict {
    Good = 0,
    BadCapabilityGap = 0x81,
    BadRuntime = 0x82,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafetyError {
    FaultLatched,
    TripActive,
    InvalidSequence,
    FanNotSafe,
    InvalidFanPercent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SafetyConfig {
    pub capabilities: u16,
}

impl SafetyConfig {
    pub const fn firmware() -> Self {
        Self {
            // The active-high trip latch is unconditional. Host silence does
            // not expire the requested outputs or authorize a new session.
            // These describe only the control paths implemented by this firmware.
            // Independent VCORE cutoff, tach interlock, and independent trip
            // monitoring are deliberately absent until hardware proves them.
            capabilities: CAP_FIVE_VOLT_CONTROL | CAP_ASIC_RESET_CONTROL | CAP_FAN_FORCE_FULL | CAP_TRIP_INPUT_SAMPLED | CAP_FAN_CONTROLLED_SPEED,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SafetyOutputs {
    pub five_volt_enabled: bool,
    pub asic_reset_asserted: bool,
    pub fan_percent: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoardControlIntent {
    pub five_volt_enable_high: bool,
    pub asic_reset_n_high: bool,
    pub fan_percent: u8,
}

impl SafetyOutputs {
    pub const SAFE: Self = Self {
        five_volt_enabled: false,
        asic_reset_asserted: true,
        fan_percent: 100,
    };

    pub const fn is_safe(self) -> bool {
        !self.five_volt_enabled && self.asic_reset_asserted && self.fan_percent == 100
    }

    pub const fn board_control_intent(self) -> BoardControlIntent {
        BoardControlIntent {
            five_volt_enable_high: self.five_volt_enabled,
            asic_reset_n_high: !self.asic_reset_asserted,
            fan_percent: self.fan_percent,
        }
    }
}

pub const fn fan_pwm_compare(percent: u8) -> u16 {
    let bounded_percent = if percent > 100 { 100 } else { percent };
    (1000u32 * bounded_percent as u32 / 100) as u16
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SafetyStatus {
    pub state: SafetyState,
    pub fault: FaultReason,
    pub runtime_verdict: RuntimeVerdict,
    pub production_verdict: ProductionVerdict,
    pub capabilities: u16,
    pub evidence: u16,
    pub outputs: SafetyOutputs,
    pub trip_input_asserted: bool,
}

impl SafetyStatus {
    pub fn encode(self) -> Vec<u8, 32> {
        let mut payload = Vec::new();
        payload
            .extend_from_slice(&[
                SAFETY_STATUS_SCHEMA_VERSION,
                SAFETY_POLICY_TRIP_LATCH_NO_LEASE,
                self.state as u8,
                self.fault as u8,
                self.runtime_verdict as u8,
                self.production_verdict as u8,
            ])
            .unwrap();
        payload.extend_from_slice(&self.capabilities.to_le_bytes()).unwrap();
        payload.extend_from_slice(&self.evidence.to_le_bytes()).unwrap();

        let output_flags = u8::from(self.outputs.five_volt_enabled) | (u8::from(self.outputs.asic_reset_asserted) << 1) | (u8::from(self.outputs.fan_percent == 100) << 2);
        payload.extend_from_slice(&[output_flags, self.outputs.fan_percent, u8::from(self.trip_input_asserted)]).unwrap();
        debug_assert_eq!(payload.len(), SAFETY_STATUS_ENCODED_LEN);
        payload
    }
}

#[derive(Debug)]
pub struct SafetyPolicy {
    config: SafetyConfig,
    fault: FaultReason,
    requested_outputs: SafetyOutputs,
    trip_input_asserted: bool,
}

impl SafetyPolicy {
    pub const fn new(config: SafetyConfig) -> Self {
        Self {
            config,
            fault: FaultReason::None,
            requested_outputs: SafetyOutputs::SAFE,
            trip_input_asserted: false,
        }
    }

    pub const fn config(&self) -> SafetyConfig {
        self.config
    }

    pub const fn outputs(&self) -> SafetyOutputs {
        if matches!(self.fault, FaultReason::None) {
            self.requested_outputs
        } else {
            SafetyOutputs::SAFE
        }
    }

    pub fn sample_trip_input(&mut self, trip_input_asserted: bool) {
        self.trip_input_asserted = trip_input_asserted;
        if trip_input_asserted {
            self.latch_fault(FaultReason::AsicTrip);
        }
    }

    pub fn request_five_volt_enabled(&mut self, enabled: bool) -> Result<(), SafetyError> {
        if !enabled {
            self.safe_off();
            return Ok(());
        }

        self.ensure_not_faulted()?;
        if self.requested_outputs.fan_percent != 100 {
            return Err(SafetyError::FanNotSafe);
        }
        if !self.requested_outputs.asic_reset_asserted {
            return Err(SafetyError::InvalidSequence);
        }
        self.requested_outputs.five_volt_enabled = true;
        Ok(())
    }

    pub fn request_asic_reset_asserted(&mut self, asserted: bool) -> Result<(), SafetyError> {
        if asserted {
            self.requested_outputs.asic_reset_asserted = true;
            return Ok(());
        }

        self.ensure_not_faulted()?;
        if !self.requested_outputs.five_volt_enabled {
            return Err(SafetyError::InvalidSequence);
        }
        self.requested_outputs.asic_reset_asserted = false;
        Ok(())
    }

    pub fn request_fan_percent(&mut self, percent: u8) -> Result<(), SafetyError> {
        if percent > 100 {
            return Err(SafetyError::InvalidFanPercent);
        }
        if percent == 100 {
            self.requested_outputs.fan_percent = percent;
            return Ok(());
        }

        self.ensure_not_faulted()?;
        self.requested_outputs.fan_percent = percent;
        Ok(())
    }

    pub fn clear_fault(&mut self) -> Result<(), SafetyError> {
        if self.trip_input_asserted {
            return Err(SafetyError::TripActive);
        }

        self.fault = FaultReason::None;
        self.requested_outputs = SafetyOutputs::SAFE;
        Ok(())
    }

    pub fn safe_off(&mut self) {
        self.requested_outputs = SafetyOutputs::SAFE;
    }

    pub fn status(&self) -> SafetyStatus {
        let outputs = self.outputs();
        // State describes effective outputs; it is not host authorization.
        let state = if !matches!(self.fault, FaultReason::None) {
            SafetyState::FaultLatched
        } else if outputs.is_safe() {
            SafetyState::SafeOff
        } else {
            SafetyState::Controlled
        };

        let mut evidence = 0;
        if outputs.is_safe() {
            evidence |= EVIDENCE_OUTPUTS_SAFE;
        }
        if !self.trip_input_asserted {
            evidence |= EVIDENCE_TRIP_CLEAR;
        }
        if matches!(self.fault, FaultReason::None) {
            evidence |= EVIDENCE_FAULT_CLEAR;
        }
        if self.config.capabilities & CAP_CORE_POWER_CUTOFF != 0 {
            evidence |= EVIDENCE_CORE_CUTOFF_AVAILABLE;
        }
        if self.config.capabilities & CAP_FAN_TACH_INTERLOCK != 0 {
            evidence |= EVIDENCE_FAN_TACH_INTERLOCK_AVAILABLE;
        }
        if self.config.capabilities & CAP_INDEPENDENT_TRIP_MONITOR != 0 {
            evidence |= EVIDENCE_INDEPENDENT_TRIP_MONITOR_AVAILABLE;
        }

        let runtime_verdict = if !matches!(self.fault, FaultReason::None) {
            RuntimeVerdict::BadFault
        } else if self.trip_input_asserted {
            RuntimeVerdict::BadTripInput
        } else if matches!(state, SafetyState::SafeOff) {
            if outputs.is_safe() {
                RuntimeVerdict::GoodSafeOff
            } else {
                RuntimeVerdict::BadUnsafeOutputs
            }
        } else {
            RuntimeVerdict::GoodControlled
        };

        let production_verdict = if !matches!(runtime_verdict, RuntimeVerdict::GoodSafeOff | RuntimeVerdict::GoodControlled) {
            ProductionVerdict::BadRuntime
        } else if self.config.capabilities & REQUIRED_PRODUCTION_CAPABILITIES != REQUIRED_PRODUCTION_CAPABILITIES {
            ProductionVerdict::BadCapabilityGap
        } else {
            ProductionVerdict::Good
        };

        SafetyStatus {
            state,
            fault: self.fault,
            runtime_verdict,
            production_verdict,
            capabilities: self.config.capabilities,
            evidence,
            outputs,
            trip_input_asserted: self.trip_input_asserted,
        }
    }

    fn ensure_not_faulted(&self) -> Result<(), SafetyError> {
        if matches!(self.fault, FaultReason::None) {
            Ok(())
        } else {
            Err(SafetyError::FaultLatched)
        }
    }

    fn latch_fault(&mut self, reason: FaultReason) {
        self.fault = reason;
        self.requested_outputs = SafetyOutputs::SAFE;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SafetyPolicy {
        SafetyPolicy::new(SafetyConfig::firmware())
    }

    fn running_policy() -> SafetyPolicy {
        let mut policy = policy();
        policy.request_five_volt_enabled(true).unwrap();
        policy.request_asic_reset_asserted(false).unwrap();
        policy.request_fan_percent(40).unwrap();
        policy
    }

    #[test]
    fn boot_requests_safe_1002_gpio_levels_and_full_fan() {
        let policy = policy();
        let status = policy.status();
        assert_eq!(status.state, SafetyState::SafeOff);
        assert_eq!(status.outputs, SafetyOutputs::SAFE);
        assert_eq!(status.runtime_verdict, RuntimeVerdict::GoodSafeOff);
        assert_ne!(status.evidence & EVIDENCE_OUTPUTS_SAFE, 0);

        let intent = status.outputs.board_control_intent();
        assert!(!intent.five_volt_enable_high);
        assert!(!intent.asic_reset_n_high);
        assert_eq!(intent.fan_percent, 100);
        assert_eq!(fan_pwm_compare(intent.fan_percent), 1000);
        assert_eq!(fan_pwm_compare(0), 0);
        assert_eq!(fan_pwm_compare(101), 1000);
    }

    #[test]
    fn direct_gpio_commands_start_and_run_without_arm_or_heartbeat() {
        let mut policy = running_policy();
        let expected = policy.status();
        assert_eq!(expected.state, SafetyState::Controlled);
        assert_eq!(expected.runtime_verdict, RuntimeVerdict::GoodControlled);
        assert_eq!(
            expected.outputs,
            SafetyOutputs {
                five_volt_enabled: true,
                asic_reset_asserted: false,
                fan_percent: 40,
            }
        );

        // The policy has no clock or deadline. Repeated safety service and
        // read-only status polls cannot expire or renew the output requests.
        for _ in 0..100_000 {
            policy.sample_trip_input(false);
            assert_eq!(policy.status(), expected);
        }
    }

    #[test]
    fn status_does_not_start_or_change_outputs() {
        let mut policy = policy();
        let initial = policy.status();
        for _ in 0..10 {
            policy.sample_trip_input(false);
            assert_eq!(policy.status(), initial);
        }
        policy.request_fan_percent(40).unwrap();
        assert_eq!(policy.status().state, SafetyState::Controlled);
        policy.request_fan_percent(100).unwrap();
        // State reflects outputs rather than an armed host session.
        assert_eq!(policy.status(), initial);
    }

    #[test]
    fn power_and_reset_ordering_still_apply_without_a_lease() {
        let mut policy = policy();
        let initial = policy.status();
        assert_eq!(policy.request_asic_reset_asserted(false), Err(SafetyError::InvalidSequence));
        assert_eq!(policy.status(), initial);
        policy.request_fan_percent(50).unwrap();
        let before = policy.status();
        assert_eq!(policy.request_five_volt_enabled(true), Err(SafetyError::FanNotSafe));
        assert_eq!(policy.status(), before);
        policy.request_fan_percent(100).unwrap();
        policy.request_five_volt_enabled(true).unwrap();
        policy.request_asic_reset_asserted(false).unwrap();
        let before = policy.status();
        assert_eq!(policy.request_five_volt_enabled(true), Err(SafetyError::InvalidSequence));
        assert_eq!(policy.status(), before);
        policy.request_fan_percent(50).unwrap();
        assert_eq!(policy.outputs().fan_percent, 50);
    }

    #[test]
    fn trip_latches_at_boot_and_while_running_until_explicitly_cleared() {
        for mut policy in [policy(), running_policy()] {
            policy.sample_trip_input(true);
            let status = policy.status();
            assert_eq!(status.state, SafetyState::FaultLatched);
            assert_eq!(status.fault, FaultReason::AsicTrip);
            assert_eq!(status.outputs, SafetyOutputs::SAFE);
            assert_eq!(status.runtime_verdict, RuntimeVerdict::BadFault);
            assert_eq!(policy.clear_fault(), Err(SafetyError::TripActive));
            policy.sample_trip_input(false);
            assert_eq!(policy.status().fault, FaultReason::AsicTrip);
            assert_eq!(policy.request_five_volt_enabled(true), Err(SafetyError::FaultLatched));
            assert_eq!(policy.request_asic_reset_asserted(false), Err(SafetyError::FaultLatched));
            assert_eq!(policy.request_fan_percent(40), Err(SafetyError::FaultLatched));
            policy.clear_fault().unwrap();
            assert_eq!(policy.status().state, SafetyState::SafeOff);
            assert_eq!(policy.status().fault, FaultReason::None);
            assert_eq!(policy.outputs(), SafetyOutputs::SAFE);
            // Recovery needs ordered GPIO commands, with no re-arm step.
            policy.request_five_volt_enabled(true).unwrap();
            policy.request_asic_reset_asserted(false).unwrap();
        }
    }

    #[test]
    fn safe_requests_remain_available_and_do_not_erase_a_latched_trip() {
        let mut policy = running_policy();
        policy.sample_trip_input(true);
        policy.request_asic_reset_asserted(true).unwrap();
        policy.request_five_volt_enabled(false).unwrap();
        policy.request_fan_percent(100).unwrap();
        policy.safe_off();
        assert_eq!(policy.outputs(), SafetyOutputs::SAFE);
        assert_eq!(policy.status().state, SafetyState::FaultLatched);
        assert_eq!(policy.status().fault, FaultReason::AsicTrip);
    }

    #[test]
    fn power_off_and_safe_off_restore_all_outputs_and_allow_direct_restart() {
        for use_safe_off in [false, true] {
            let mut policy = running_policy();
            if use_safe_off {
                policy.safe_off();
            } else {
                policy.request_five_volt_enabled(false).unwrap();
            }
            assert_eq!(policy.outputs(), SafetyOutputs::SAFE);
            assert_eq!(policy.status().state, SafetyState::SafeOff);
            policy.request_five_volt_enabled(true).unwrap();
            policy.request_asic_reset_asserted(false).unwrap();
            assert_eq!(policy.status().state, SafetyState::Controlled);
        }
    }

    #[test]
    fn invalid_fan_requests_do_not_change_the_outputs() {
        let mut policy = running_policy();
        let before = policy.status();
        for percent in [101, 255] {
            assert_eq!(policy.request_fan_percent(percent), Err(SafetyError::InvalidFanPercent));
            assert_eq!(policy.status(), before);
        }
    }

    #[test]
    fn current_capabilities_retain_1002_controls_without_claiming_hardware_gates() {
        let policy = policy();
        assert_eq!(policy.config().capabilities, 0x008f);
        assert_eq!(policy.status().production_verdict, ProductionVerdict::BadCapabilityGap);
        assert_eq!(policy.config().capabilities & CAP_CORE_POWER_CUTOFF, 0);
        assert_eq!(policy.config().capabilities & CAP_FAN_TACH_INTERLOCK, 0);
        assert_eq!(policy.config().capabilities & CAP_INDEPENDENT_TRIP_MONITOR, 0);
    }

    #[test]
    fn production_verdict_still_requires_capabilities_and_good_runtime() {
        let mut policy = SafetyPolicy::new(SafetyConfig { capabilities: REQUIRED_PRODUCTION_CAPABILITIES });
        assert_eq!(policy.status().production_verdict, ProductionVerdict::Good);
        policy.sample_trip_input(true);
        assert_eq!(policy.status().production_verdict, ProductionVerdict::BadRuntime);
    }

    #[test]
    fn schema_two_encodes_outputs_and_faults_without_lease_fields_or_evidence() {
        // Literal wire vectors catch accidental offset changes in ESP's new decoder.
        let mut policy = policy();
        assert_eq!(policy.status().encode().as_slice(), &[2, 3, 0, 0, 0, 0x81, 0x8f, 0, 0x0d, 0, 6, 100, 0]);
        policy = running_policy();
        assert_eq!(policy.status().encode().as_slice(), &[2, 3, 1, 0, 1, 0x81, 0x8f, 0, 0x0c, 0, 1, 40, 0]);
        policy.sample_trip_input(true);
        assert_eq!(policy.status().encode().as_slice(), &[2, 3, 2, 2, 0x80, 0x82, 0x8f, 0, 1, 0, 6, 100, 1]);
        assert_eq!(policy.status().encode().len(), SAFETY_STATUS_ENCODED_LEN);
    }

    #[test]
    fn supported_wire_commands_are_exact_and_legacy_lease_commands_are_rejected() {
        for (opcode, command) in [(0x10, SafetyWireCommand::GetStatus), (0x13, SafetyWireCommand::ClearFault), (0x14, SafetyWireCommand::SafeOff)] {
            assert_eq!(decode_wire_command(&[opcode]), Some(command));
            assert_eq!(decode_wire_command(&[opcode, 0]), None);
        }
        for bytes in [&[][..], &[0x11], &[0x12], &[0x11, 0], &[0x12, 0], &[0xff]] {
            assert_eq!(decode_wire_command(bytes), None);
        }
    }
}

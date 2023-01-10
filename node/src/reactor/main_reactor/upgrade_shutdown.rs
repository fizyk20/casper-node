use std::time::Duration;

use crate::{
    effect::{announcements::ControlAnnouncement, EffectBuilder, EffectExt, Effects},
    reactor::main_reactor::{MainEvent, MainReactor},
    types::EraValidatorWeights,
};

const DELAY_BEFORE_SHUTDOWN: Duration = Duration::from_secs(2);

pub(super) enum UpgradeShutdownInstruction {
    Do(Duration, Effects<MainEvent>),
    CheckLater(String, Duration),
    Fatal(String),
}

impl MainReactor {
    pub(super) fn upgrade_shutdown_instruction(
        &self,
        effect_builder: EffectBuilder<MainEvent>,
    ) -> UpgradeShutdownInstruction {
        let recent_switch_block_headers = match self.storage.read_highest_switch_block_headers(1) {
            Ok(headers) => headers,
            Err(error) => {
                return UpgradeShutdownInstruction::Fatal(format!(
                    "error getting recent switch block headers: {}",
                    error
                ))
            }
        };
        if let Some(block_header) = recent_switch_block_headers.last() {
            let highest_switch_block_era = block_header.era_id();
            return match self
                .validator_matrix
                .validator_weights(highest_switch_block_era)
            {
                Some(validator_weights) => self.upgrade_shutdown_has_sufficient_finality(
                    effect_builder,
                    &validator_weights,
                    block_header.height(),
                ),
                None => UpgradeShutdownInstruction::Fatal(
                    "validator_weights cannot be missing".to_string(),
                ),
            };
        }
        UpgradeShutdownInstruction::Fatal("recent_switch_block_headers cannot be empty".to_string())
    }

    fn upgrade_shutdown_has_sufficient_finality(
        &self,
        effect_builder: EffectBuilder<MainEvent>,
        validator_weights: &EraValidatorWeights,
        highest_block_height: u64,
    ) -> UpgradeShutdownInstruction {
        match self
            .storage
            .era_has_sufficient_finality_signatures(validator_weights)
        {
            Ok(true)
                if self.storage.highest_complete_block_height() == Some(highest_block_height) =>
            {
                // Allow a delay to acquire more finality signatures
                let effects = effect_builder
                    .set_timeout(DELAY_BEFORE_SHUTDOWN)
                    .event(|_| {
                        MainEvent::ControlAnnouncement(ControlAnnouncement::ShutdownForUpgrade)
                    });
                // should not need to crank the control logic again as the reactor will shutdown
                UpgradeShutdownInstruction::Do(DELAY_BEFORE_SHUTDOWN, effects)
            }
            Ok(_) => UpgradeShutdownInstruction::CheckLater(
                "waiting for sufficient finality and completion of all blocks".to_string(),
                DELAY_BEFORE_SHUTDOWN,
            ),
            Err(error) => UpgradeShutdownInstruction::Fatal(format!(
                "failed check for sufficient finality signatures: {}",
                error
            )),
        }
    }
}

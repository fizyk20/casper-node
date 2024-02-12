use std::collections::BTreeSet;

use datasize::DataSize;
use serde::{Deserialize, Serialize};

use casper_types::{Deploy, DeployApproval, DeployHash};

/// The hash of a deploy (or transfer) together with signatures approving it for execution.
#[derive(Clone, DataSize, Debug, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeployHashWithApprovals {
    /// The hash of the deploy.
    deploy_hash: DeployHash,
    /// The approvals for the deploy.
    approvals: BTreeSet<DeployApproval>,
}

impl DeployHashWithApprovals {
    /// Creates a new `DeployWithApprovals`.
    pub(crate) fn new(deploy_hash: DeployHash, approvals: BTreeSet<DeployApproval>) -> Self {
        Self {
            deploy_hash,
            approvals,
        }
    }

    /// Returns the deploy hash.
    pub(crate) fn deploy_hash(&self) -> &DeployHash {
        &self.deploy_hash
    }

    /// Returns the approvals.
    pub(crate) fn approvals(&self) -> &BTreeSet<DeployApproval> {
        &self.approvals
    }
}

impl From<&Deploy> for DeployHashWithApprovals {
    fn from(deploy: &Deploy) -> Self {
        DeployHashWithApprovals {
            deploy_hash: *deploy.hash(),
            approvals: deploy.approvals().clone(),
        }
    }
}
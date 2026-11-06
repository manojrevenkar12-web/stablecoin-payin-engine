//! Transaction statuses, named as in MoonPay Enterprise's public API, and the transitions allowed
//! between them. Any other transition is a bug and is refused.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Status {
    /// Received and held for a compliance or fraud check, or waiting for the mint window.
    FundsReviewInProgress,
    ConversionInProgress,
    /// Mint submitted on-chain, or fiat payout sent to the bank.
    PayoutInProgress,
    Completed,
    Failed,
    RejectedAml,
    RejectedFraud,
    RejectedMinAmount,
    /// Not in the public status list: a held deposit that operations decided to send back
    /// (for example a customer's accidental second payment). Terminal.
    Returned,
}

impl Status {
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Completed | Status::Failed | Status::RejectedAml | Status::RejectedFraud | Status::RejectedMinAmount | Status::Returned)
    }

    pub fn can_move_to(self, to: Status) -> bool {
        use Status::*;
        matches!(
            (self, to),
            (FundsReviewInProgress, ConversionInProgress)
                | (FundsReviewInProgress, RejectedAml)
                | (FundsReviewInProgress, RejectedFraud)
                | (FundsReviewInProgress, Returned)
                | (FundsReviewInProgress, RejectedMinAmount)
                | (ConversionInProgress, PayoutInProgress)
                | (PayoutInProgress, Completed)
                | (PayoutInProgress, Failed)
                // A payout the recipient's bank sends back after completion.
                | (Completed, Failed)
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct BadTransition {
    pub from: Status,
    pub to: Status,
}

impl std::fmt::Display for BadTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "transition {:?} -> {:?} is not allowed", self.from, self.to)
    }
}
impl std::error::Error for BadTransition {}

pub fn check(from: Status, to: Status) -> Result<(), BadTransition> {
    if from.can_move_to(to) { Ok(()) } else { Err(BadTransition { from, to }) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Status::*;

    #[test]
    fn forward_path_and_bank_returns() {
        for (a, b) in
            [(FundsReviewInProgress, ConversionInProgress), (ConversionInProgress, PayoutInProgress), (PayoutInProgress, Completed), (Completed, Failed)]
        {
            assert!(check(a, b).is_ok());
        }
    }

    #[test]
    fn no_way_back_from_rejection_and_no_skipping() {
        assert!(check(RejectedAml, ConversionInProgress).is_err());
        assert!(check(FundsReviewInProgress, Completed).is_err());
        assert!(check(Failed, Completed).is_err());
        assert!(check(Returned, ConversionInProgress).is_err());
        let all = [FundsReviewInProgress, ConversionInProgress, PayoutInProgress, Completed, Failed, RejectedAml, RejectedFraud, RejectedMinAmount, Returned];
        for s in all {
            assert!(!s.can_move_to(s), "{s:?} -> itself");
        }
        assert!(all.iter().filter(|s| s.is_terminal()).all(|s| *s == Completed || all.iter().all(|t| !s.can_move_to(*t))));
    }
}

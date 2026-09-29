//! Failure classification (Phase 4).
//!
//! This is a TAXONOMY over the error types the project already has
//! (`MathError`, `RevalidationError`, `TickArrayError`, `CpmmQuoteError`, and
//! the executor's `anyhow` errors) - not a second error system. Every existing
//! error keeps its own type and message; `FailureClass` only answers "what
//! kind of failure is this, and what should an operator/caller do about it".
//!
//! The one genuinely new error type is [`SimulationFailure`]. The executor used
//! to collapse every simulation failure into an untyped string, which made a
//! chain error like `InstructionError(5, Custom(6005))` impossible to
//! classify. It now carries the typed `TransactionError` plus the instruction
//! index and the program that instruction invoked.
//!
//! ## Why classification is keyed on (program, code), never on code alone
//!
//! Custom error codes are per-program and collide. `6005` is
//! `ExceededSlippage` in Raydium CPMM but `ClosePositionNotEmpty` in the Orca
//! Whirlpool program. A global "6005 => slippage" rule would misclassify an
//! Orca failure. So the failing instruction's program is resolved first.
//!
//! Code tables were verified against:
//!   - Raydium CPMM: raydium-io/raydium-cp-swap `error.rs` (6000-based numbering
//!     confirmed via the raydium-cp-swap-cpi crate).
//!   - Orca Whirlpool: the IDL-derived `WhirlpoolError` table (cross-checked
//!     across three independent generated mirrors). Worth re-checking against
//!     Orca's own `errors.rs` if Orca ships a new program version.
//!
//! Codes not listed below deliberately fall through to `SimulationError`
//! with the raw code preserved in the message - the classifier never
//! guesses.

use crate::analyzer::{MathError, RevalidationError};
use crate::scanner::orca::TickArrayError;
use crate::scanner::raydium_cpmm::CpmmQuoteError;
use solana_sdk::instruction::InstructionError;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::TransactionError;
use std::str::FromStr;

/// What kind of failure occurred.
///
/// The `as_str` values are stable identifiers used in logs (and later in
/// metrics); do not rename them without treating it as a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureClass {
    /// Our snapshot is old, incoherent, or not yet available. Wait/re-fetch.
    StaleState,
    /// Our quote and the chain disagree in a way that is not just price
    /// movement (e.g. a partial fill). Investigate - possible math bug.
    QuoteMismatch,
    /// The chain refused the trade because output fell below our minimum.
    /// Normal market movement between quote and execution: discard.
    ExceededSlippage,
    /// An account we depend on is gone or changed underneath us.
    AccountStateChanged,
    /// Transport-level RPC failure. Infrastructure, not market.
    RpcError,
    /// The chain rejected the transaction for a reason we do not classify
    /// more precisely. The raw error is always preserved.
    SimulationError,
    /// The pool (or an account we decoded) is malformed or not tradable.
    InvalidPoolState,
    /// Tick-array coverage is missing, mis-sequenced, or incomplete.
    MissingTickArray,
    /// Our own integer arithmetic overflowed or was fed a degenerate value.
    ArithmeticError,
    /// We built or ran something incorrectly (bad account, budget exceeded,
    /// or an error we have no better bucket for).
    ExecutionError,
}

impl FailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            FailureClass::StaleState => "stale_state",
            FailureClass::QuoteMismatch => "quote_mismatch",
            FailureClass::ExceededSlippage => "exceeded_slippage",
            FailureClass::AccountStateChanged => "account_state_changed",
            FailureClass::RpcError => "rpc_error",
            FailureClass::SimulationError => "simulation_error",
            FailureClass::InvalidPoolState => "invalid_pool_state",
            FailureClass::MissingTickArray => "missing_tick_array",
            FailureClass::ArithmeticError => "arithmetic_error",
            FailureClass::ExecutionError => "execution_error",
        }
    }
}

impl std::fmt::Display for FailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Program identification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownProgram {
    OrcaWhirlpool,
    RaydiumCpmm,
    RaydiumAmmV4,
    /// Anything else (System, Token, ComputeBudget, ...). Custom codes from
    /// these are not interpreted.
    Other,
}

/// Resolves a program id to a program we know the error table of. Program ids
/// come from the same constants the executor uses to build instructions, so
/// the classifier cannot drift from what is actually submitted.
pub fn identify_program(program_id: &Pubkey) -> KnownProgram {
    let matches = |s: &str| {
        Pubkey::from_str(s)
            .map(|p| p == *program_id)
            .unwrap_or(false)
    };

    if matches(crate::executor::orca::ORCA_WHIRLPOOL_PROGRAM) {
        KnownProgram::OrcaWhirlpool
    } else if matches(crate::executor::raydium_cpmm::RAYDIUM_CPMM_PROGRAM) {
        KnownProgram::RaydiumCpmm
    } else if matches(crate::executor::raydium::RAYDIUM_AMM_V4_PROGRAM) {
        KnownProgram::RaydiumAmmV4
    } else {
        KnownProgram::Other
    }
}

/// Maps a program's `Custom(code)` to a failure class.
///
/// Only codes whose meaning is established are mapped. Everything else,
/// including every Raydium AMM v4 code (its numbering is a different scheme
/// and is intentionally not interpreted), is `SimulationError`.
pub fn classify_program_error(program: KnownProgram, code: u32) -> FailureClass {
    match program {
        KnownProgram::RaydiumCpmm => match code {
            // ExceededSlippage - "Exceeds desired slippage limit".
            6005 => FailureClass::ExceededSlippage,
            // NotApproved: swap not currently allowed on this pool.
            6000 => FailureClass::InvalidPoolState,
            // NotSupportMint: "Not support token_2022 mint extension".
            6007 => FailureClass::InvalidPoolState,
            _ => FailureClass::SimulationError,
        },
        KnownProgram::OrcaWhirlpool => match code {
            // AmountOutBelowMinimum / AmountInAboveMaximum: Orca's swap
            // slippage checks. (NOT 6005 - that is ClosePositionNotEmpty.)
            6036 | 6037 => FailureClass::ExceededSlippage,
            // TickArrayIndexOutofBounds, InvalidTickArraySequence,
            // TickArraySequenceInvalidIndex: the tick-array set we supplied
            // does not cover the swap.
            6003 | 6023 | 6038 => FailureClass::MissingTickArray,
            // PartialFillError: the trade could not be fully filled, so the
            // liquidity our quote assumed was not there.
            6057 => FailureClass::QuoteMismatch,
            // TradeIsNotEnabled.
            6064 => FailureClass::InvalidPoolState,
            // DivideByZero, NumberCastError, NumberDownCastError,
            // LiquidityOverflow, LiquidityUnderflow, and the multiplication /
            // muldiv / amount overflow family.
            6006 | 6007 | 6008 | 6014 | 6015 | 6030 | 6031 | 6032 | 6033 | 6039 | 6040 => {
                FailureClass::ArithmeticError
            }
            // DifferentWhirlpoolTickArrayAccount: we passed a tick array that
            // belongs to another pool - an instruction-construction fault.
            6056 => FailureClass::ExecutionError,
            _ => FailureClass::SimulationError,
        },
        KnownProgram::RaydiumAmmV4 | KnownProgram::Other => FailureClass::SimulationError,
    }
}

/// Classifies a chain-returned `TransactionError`, given the program that
/// owned the failing instruction (if any).
pub fn classify_transaction_error(err: &TransactionError, program: KnownProgram) -> FailureClass {
    match err {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => {
            classify_program_error(program, *code)
        }
        TransactionError::InstructionError(_, InstructionError::ComputationalBudgetExceeded) => {
            FailureClass::ExecutionError
        }
        // The blockhash we signed with is no longer valid: our cached state
        // is stale.
        TransactionError::BlockhashNotFound => FailureClass::StaleState,
        // An account the transaction needs no longer exists.
        TransactionError::AccountNotFound => FailureClass::AccountStateChanged,
        _ => FailureClass::SimulationError,
    }
}

// ---------------------------------------------------------------------------
// Simulation truth gate
// ---------------------------------------------------------------------------

/// A failed simulation. The two variants are the two ways a simulation can
/// fail, and BOTH are failures: transport errors and chain-reported errors.
#[derive(Debug, thiserror::Error)]
pub enum SimulationFailure {
    /// The RPC call itself failed; nothing was learned about the transaction.
    #[error("RPC transport error during simulation: {0}")]
    Rpc(String),
    /// The RPC call succeeded but the simulated transaction returned an error
    /// (`RpcSimulateTransactionResult.err`). This is the case a naive
    /// `.is_ok()` check would miss.
    #[error(
        "simulated transaction failed at instruction {instruction_index:?} \
         (program {program:?}, id {program_id:?}): {error:?}"
    )]
    Chain {
        instruction_index: Option<u8>,
        program: KnownProgram,
        program_id: Option<Pubkey>,
        error: TransactionError,
    },
}

impl SimulationFailure {
    pub fn class(&self) -> FailureClass {
        match self {
            SimulationFailure::Rpc(_) => FailureClass::RpcError,
            SimulationFailure::Chain { error, program, .. } => {
                classify_transaction_error(error, *program)
            }
        }
    }
}

/// THE simulation truth gate.
///
/// A simulation succeeds ONLY when the RPC transport succeeded (the caller
/// only reaches this function with a response in hand) AND the response's
/// `value.err` is `None`. Any `Some(err)` is a failure, with the failing
/// instruction's program resolved from `instruction_programs` (the program id
/// of each instruction in submission order; message compilation preserves
/// instruction order, so the index in `InstructionError` indexes this slice).
///
/// Kept as a pure function so the gate itself is unit-testable without a
/// network.
pub fn simulation_outcome(
    value_err: Option<TransactionError>,
    instruction_programs: &[Pubkey],
) -> Result<(), SimulationFailure> {
    let Some(error) = value_err else {
        return Ok(());
    };

    let instruction_index = match &error {
        TransactionError::InstructionError(idx, _) => Some(*idx),
        _ => None,
    };

    let program_id = instruction_index
        .and_then(|idx| instruction_programs.get(idx as usize))
        .copied();

    let program = program_id
        .as_ref()
        .map(identify_program)
        .unwrap_or(KnownProgram::Other);

    Err(SimulationFailure::Chain {
        instruction_index,
        program,
        program_id,
        error,
    })
}

// ---------------------------------------------------------------------------
// Classification of the project's existing error types
// ---------------------------------------------------------------------------

/// Implemented by every existing error type that participates in
/// classification.
pub trait Classify {
    fn failure_class(&self) -> FailureClass;
}

impl Classify for SimulationFailure {
    fn failure_class(&self) -> FailureClass {
        self.class()
    }
}

impl Classify for CpmmQuoteError {
    fn failure_class(&self) -> FailureClass {
        match self {
            CpmmQuoteError::ZeroReserve
            | CpmmQuoteError::InvalidCreatorFeeMode
            | CpmmQuoteError::Token2022Unsupported
            | CpmmQuoteError::SwapDisabled
            | CpmmQuoteError::PoolNotOpen => FailureClass::InvalidPoolState,
            CpmmQuoteError::ReserveFeeUnderflow
            | CpmmQuoteError::ArithmeticOverflow
            | CpmmQuoteError::OutputTooLarge
            // Fees consumed the entire input: a degenerate numeric outcome.
            | CpmmQuoteError::InsufficientInputAfterFees => FailureClass::ArithmeticError,
        }
    }
}

impl Classify for TickArrayError {
    fn failure_class(&self) -> FailureClass {
        match self {
            TickArrayError::MissingNeighbor { .. } => FailureClass::MissingTickArray,
            TickArrayError::TooShort { .. }
            | TickArrayError::WrongOwner { .. }
            | TickArrayError::WrongWhirlpool { .. }
            | TickArrayError::InvalidStartTick { .. }
            | TickArrayError::TickDecode { .. } => FailureClass::InvalidPoolState,
        }
    }
}

impl Classify for RevalidationError {
    fn failure_class(&self) -> FailureClass {
        match self {
            RevalidationError::NoSnapshot
            | RevalidationError::Incoherent { .. }
            | RevalidationError::SnapshotTooOld { .. } => FailureClass::StaleState,
            RevalidationError::CurrentTickArrayMissing { .. }
            | RevalidationError::ExecutionArrayMissing { .. } => FailureClass::MissingTickArray,
        }
    }
}

impl Classify for MathError {
    fn failure_class(&self) -> FailureClass {
        match self {
            MathError::ZeroReserve => FailureClass::InvalidPoolState,
            MathError::Overflow(_)
            | MathError::DivisionByZero(_)
            | MathError::OutputTooLarge
            | MathError::InvalidInput(_) => FailureClass::ArithmeticError,
            // No published state yet: not an error in the math, the state is
            // simply not ready.
            MathError::OrcaQuoteUnavailable(_)
            | MathError::CpmmQuoteStateUnavailable
            | MathError::OrcaStateIncoherent { .. } => FailureClass::StaleState,
            MathError::OrcaInsufficientTickCoverage => FailureClass::MissingTickArray,
            MathError::CpmmQuote(e) => e.failure_class(),
            MathError::OrcaQuoteFailed(msg) => classify_orca_core_error(msg),
        }
    }
}

/// Classifies an `orca_whirlpools_core` quote error by matching against the
/// crate's own exported error constants (not copied string literals), so a
/// wording change in the crate is a compile error here rather than a silent
/// misclassification.
fn classify_orca_core_error(msg: &str) -> FailureClass {
    use orca_whirlpools_core as core;

    if msg == core::INVALID_TICK_ARRAY_SEQUENCE
        || msg == core::TICK_SEQUENCE_EMPTY
        || msg == core::TICK_INDEX_NOT_IN_ARRAY
        || msg == core::TICK_ARRAY_NOT_EVENLY_SPACED
    {
        FailureClass::MissingTickArray
    } else if msg == core::ARITHMETIC_OVERFLOW
        || msg == core::AMOUNT_EXCEEDS_MAX_U64
        || msg == core::ZERO_TRADABLE_AMOUNT
    {
        FailureClass::ArithmeticError
    } else if msg == core::SQRT_PRICE_OUT_OF_BOUNDS
        || msg == core::SQRT_PRICE_LIMIT_OUT_OF_BOUNDS
        || msg == core::TICK_INDEX_OUT_OF_BOUNDS
        || msg == core::INVALID_TICK_INDEX
        // We pass no oracle; an adaptive-fee pool reaching here means the pool
        // is not what we assumed.
        || msg == core::INVALID_ADAPTIVE_FEE_INFO
    {
        FailureClass::InvalidPoolState
    } else {
        FailureClass::ExecutionError
    }
}

/// Classifies an arbitrary `anyhow::Error` by walking its cause chain and
/// returning the class of the first recognised typed error. Context wrappers
/// added with `.context(...)` do not hide the underlying typed error.
///
/// Errors with no recognised type default to `ExecutionError`, except a
/// `solana_client` `ClientError` anywhere in the chain, which is `RpcError`.
pub fn classify_error_chain(err: &anyhow::Error) -> FailureClass {
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<SimulationFailure>() {
            return e.failure_class();
        }
        if let Some(e) = cause.downcast_ref::<MathError>() {
            return e.failure_class();
        }
        if let Some(e) = cause.downcast_ref::<RevalidationError>() {
            return e.failure_class();
        }
        if let Some(e) = cause.downcast_ref::<TickArrayError>() {
            return e.failure_class();
        }
        if cause
            .downcast_ref::<solana_client::client_error::ClientError>()
            .is_some()
        {
            return FailureClass::RpcError;
        }
    }
    FailureClass::ExecutionError
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(deprecated)] // no non-deprecated re-export exists in this solana-sdk version
    use solana_sdk::compute_budget;
    #[allow(deprecated)]
    use solana_sdk::system_program;

    fn pk(s: &str) -> Pubkey {
        Pubkey::from_str(s).unwrap()
    }
    fn orca() -> Pubkey {
        pk(crate::executor::orca::ORCA_WHIRLPOOL_PROGRAM)
    }
    fn cpmm() -> Pubkey {
        pk(crate::executor::raydium_cpmm::RAYDIUM_CPMM_PROGRAM)
    }

    /// Program ids in the exact order Falcon's atomic transaction is built
    /// when both ATAs already exist: 0=CU limit, 1=CU price, 2=SOL transfer,
    /// 3=sync_native, 4=buy leg, 5=sell leg.
    fn atomic_tx_programs(buy: Pubkey, sell: Pubkey) -> Vec<Pubkey> {
        vec![
            compute_budget::id(),
            compute_budget::id(),
            system_program::id(),
            spl_token::id(),
            buy,
            sell,
        ]
    }

    // --- taxonomy identity ---

    #[test]
    fn failure_class_strings_are_stable() {
        // Logs and future metrics key on these exact strings.
        let all = [
            (FailureClass::StaleState, "stale_state"),
            (FailureClass::QuoteMismatch, "quote_mismatch"),
            (FailureClass::ExceededSlippage, "exceeded_slippage"),
            (FailureClass::AccountStateChanged, "account_state_changed"),
            (FailureClass::RpcError, "rpc_error"),
            (FailureClass::SimulationError, "simulation_error"),
            (FailureClass::InvalidPoolState, "invalid_pool_state"),
            (FailureClass::MissingTickArray, "missing_tick_array"),
            (FailureClass::ArithmeticError, "arithmetic_error"),
            (FailureClass::ExecutionError, "execution_error"),
        ];
        for (class, s) in all {
            assert_eq!(class.as_str(), s);
            assert_eq!(class.to_string(), s);
        }
    }

    #[test]
    fn program_ids_resolve_to_known_programs() {
        assert_eq!(identify_program(&orca()), KnownProgram::OrcaWhirlpool);
        assert_eq!(identify_program(&cpmm()), KnownProgram::RaydiumCpmm);
        assert_eq!(
            identify_program(&pk(crate::executor::raydium::RAYDIUM_AMM_V4_PROGRAM)),
            KnownProgram::RaydiumAmmV4
        );
        assert_eq!(identify_program(&system_program::id()), KnownProgram::Other);
    }

    // --- (program, code) mapping ---

    #[test]
    fn cpmm_6005_is_exceeded_slippage() {
        assert_eq!(
            classify_program_error(KnownProgram::RaydiumCpmm, 6005),
            FailureClass::ExceededSlippage
        );
    }

    #[test]
    fn orca_6005_is_not_slippage() {
        // The regression guard for the whole design: the SAME code means
        // ClosePositionNotEmpty on Orca and must never read as slippage.
        assert_ne!(
            classify_program_error(KnownProgram::OrcaWhirlpool, 6005),
            FailureClass::ExceededSlippage
        );
        assert_eq!(
            classify_program_error(KnownProgram::OrcaWhirlpool, 6005),
            FailureClass::SimulationError
        );
    }

    #[test]
    fn orca_slippage_codes_are_6036_and_6037() {
        for code in [6036, 6037] {
            assert_eq!(
                classify_program_error(KnownProgram::OrcaWhirlpool, code),
                FailureClass::ExceededSlippage
            );
        }
        // And they are not slippage on Raydium CPMM.
        assert_ne!(
            classify_program_error(KnownProgram::RaydiumCpmm, 6036),
            FailureClass::ExceededSlippage
        );
    }

    #[test]
    fn orca_tick_array_codes_are_missing_tick_array() {
        for code in [6003, 6023, 6038] {
            assert_eq!(
                classify_program_error(KnownProgram::OrcaWhirlpool, code),
                FailureClass::MissingTickArray,
                "code {code}"
            );
        }
    }

    #[test]
    fn orca_other_mapped_codes() {
        let o = KnownProgram::OrcaWhirlpool;
        assert_eq!(classify_program_error(o, 6057), FailureClass::QuoteMismatch);
        assert_eq!(
            classify_program_error(o, 6064),
            FailureClass::InvalidPoolState
        );
        assert_eq!(
            classify_program_error(o, 6056),
            FailureClass::ExecutionError
        );
        for code in [
            6006, 6007, 6008, 6014, 6015, 6030, 6031, 6032, 6033, 6039, 6040,
        ] {
            assert_eq!(
                classify_program_error(o, code),
                FailureClass::ArithmeticError,
                "code {code}"
            );
        }
    }

    #[test]
    fn cpmm_other_mapped_codes() {
        let c = KnownProgram::RaydiumCpmm;
        assert_eq!(
            classify_program_error(c, 6000),
            FailureClass::InvalidPoolState
        );
        assert_eq!(
            classify_program_error(c, 6007),
            FailureClass::InvalidPoolState
        );
    }

    #[test]
    fn unknown_codes_and_programs_are_not_guessed() {
        assert_eq!(
            classify_program_error(KnownProgram::RaydiumCpmm, 6999),
            FailureClass::SimulationError
        );
        assert_eq!(
            classify_program_error(KnownProgram::OrcaWhirlpool, 1),
            FailureClass::SimulationError
        );
        // An unknown program's 6005 must not be read as slippage either.
        assert_eq!(
            classify_program_error(KnownProgram::Other, 6005),
            FailureClass::SimulationError
        );
        // Raydium AMM v4 codes are not interpreted at all.
        assert_eq!(
            classify_program_error(KnownProgram::RaydiumAmmV4, 6005),
            FailureClass::SimulationError
        );
    }

    // --- transaction-level errors ---

    #[test]
    fn non_custom_transaction_errors() {
        let o = KnownProgram::Other;
        assert_eq!(
            classify_transaction_error(&TransactionError::BlockhashNotFound, o),
            FailureClass::StaleState
        );
        assert_eq!(
            classify_transaction_error(&TransactionError::AccountNotFound, o),
            FailureClass::AccountStateChanged
        );
        assert_eq!(
            classify_transaction_error(
                &TransactionError::InstructionError(
                    3,
                    InstructionError::ComputationalBudgetExceeded
                ),
                o
            ),
            FailureClass::ExecutionError
        );
        assert_eq!(
            classify_transaction_error(&TransactionError::AlreadyProcessed, o),
            FailureClass::SimulationError
        );
    }

    // --- the simulation truth gate ---

    #[test]
    fn simulation_with_no_chain_error_succeeds() {
        assert!(simulation_outcome(None, &atomic_tx_programs(orca(), cpmm())).is_ok());
    }

    #[test]
    fn any_chain_error_is_a_failed_simulation() {
        // Even an "uninteresting" error must fail the gate: transport success
        // alone is not simulation success.
        let err = TransactionError::AlreadyProcessed;
        assert!(simulation_outcome(Some(err), &[]).is_err());
    }

    #[test]
    fn the_real_observed_failure_classifies_as_exceeded_slippage() {
        // The failure that started this phase: Orca buy succeeded, then
        // InstructionError(5, Custom(6005)) on the Raydium CPMM sell.
        let programs = atomic_tx_programs(orca(), cpmm());
        let failure = simulation_outcome(
            Some(TransactionError::InstructionError(
                5,
                InstructionError::Custom(6005),
            )),
            &programs,
        )
        .unwrap_err();

        match &failure {
            SimulationFailure::Chain {
                instruction_index,
                program,
                program_id,
                ..
            } => {
                assert_eq!(*instruction_index, Some(5));
                assert_eq!(*program, KnownProgram::RaydiumCpmm);
                assert_eq!(*program_id, Some(cpmm()));
            }
            other => panic!("expected Chain failure, got {other:?}"),
        }
        assert_eq!(failure.class(), FailureClass::ExceededSlippage);
    }

    #[test]
    fn same_code_on_the_orca_leg_is_not_slippage() {
        // Identical error, but at index 4 - the Orca buy leg.
        let programs = atomic_tx_programs(orca(), cpmm());
        let failure = simulation_outcome(
            Some(TransactionError::InstructionError(
                4,
                InstructionError::Custom(6005),
            )),
            &programs,
        )
        .unwrap_err();
        assert_ne!(failure.class(), FailureClass::ExceededSlippage);
    }

    #[test]
    fn program_resolution_follows_leg_order() {
        // Reversed route (Raydium buy, Orca sell): index 5 is now Orca, and
        // its 6036 is the slippage code.
        let programs = atomic_tx_programs(cpmm(), orca());
        let failure = simulation_outcome(
            Some(TransactionError::InstructionError(
                5,
                InstructionError::Custom(6036),
            )),
            &programs,
        )
        .unwrap_err();
        assert_eq!(failure.class(), FailureClass::ExceededSlippage);
    }

    #[test]
    fn out_of_range_instruction_index_does_not_panic() {
        let failure = simulation_outcome(
            Some(TransactionError::InstructionError(
                200,
                InstructionError::Custom(6005),
            )),
            &atomic_tx_programs(orca(), cpmm()),
        )
        .unwrap_err();
        // Program unresolvable -> the code is not interpreted.
        assert_eq!(failure.class(), FailureClass::SimulationError);
    }

    #[test]
    fn rpc_transport_failure_is_rpc_error() {
        assert_eq!(
            SimulationFailure::Rpc("connection reset".into()).class(),
            FailureClass::RpcError
        );
    }

    // --- existing error types ---

    #[test]
    fn math_error_classification_table() {
        use MathError::*;
        let cases: Vec<(MathError, FailureClass)> = vec![
            (ZeroReserve, FailureClass::InvalidPoolState),
            (Overflow("x"), FailureClass::ArithmeticError),
            (DivisionByZero("x"), FailureClass::ArithmeticError),
            (OutputTooLarge, FailureClass::ArithmeticError),
            (InvalidInput("x"), FailureClass::ArithmeticError),
            (OrcaQuoteUnavailable("x"), FailureClass::StaleState),
            (CpmmQuoteStateUnavailable, FailureClass::StaleState),
            (
                OrcaStateIncoherent { worst_drift: 9 },
                FailureClass::StaleState,
            ),
            (OrcaInsufficientTickCoverage, FailureClass::MissingTickArray),
            (
                CpmmQuote(CpmmQuoteError::SwapDisabled),
                FailureClass::InvalidPoolState,
            ),
            (
                CpmmQuote(CpmmQuoteError::ArithmeticOverflow),
                FailureClass::ArithmeticError,
            ),
        ];
        for (err, want) in cases {
            assert_eq!(err.failure_class(), want, "{err:?}");
        }
    }

    #[test]
    fn orca_core_errors_are_classified_via_the_crates_own_constants() {
        use orca_whirlpools_core as c;
        let cases = [
            (
                c::INVALID_TICK_ARRAY_SEQUENCE,
                FailureClass::MissingTickArray,
            ),
            (c::TICK_SEQUENCE_EMPTY, FailureClass::MissingTickArray),
            (c::TICK_INDEX_NOT_IN_ARRAY, FailureClass::MissingTickArray),
            (
                c::TICK_ARRAY_NOT_EVENLY_SPACED,
                FailureClass::MissingTickArray,
            ),
            (c::ARITHMETIC_OVERFLOW, FailureClass::ArithmeticError),
            (c::AMOUNT_EXCEEDS_MAX_U64, FailureClass::ArithmeticError),
            (c::ZERO_TRADABLE_AMOUNT, FailureClass::ArithmeticError),
            (c::SQRT_PRICE_OUT_OF_BOUNDS, FailureClass::InvalidPoolState),
            (
                c::SQRT_PRICE_LIMIT_OUT_OF_BOUNDS,
                FailureClass::InvalidPoolState,
            ),
            (c::TICK_INDEX_OUT_OF_BOUNDS, FailureClass::InvalidPoolState),
            (c::INVALID_TICK_INDEX, FailureClass::InvalidPoolState),
            (c::INVALID_ADAPTIVE_FEE_INFO, FailureClass::InvalidPoolState),
            // Unrecognised core error: falls to execution_error, not guessed.
            ("some future core error", FailureClass::ExecutionError),
        ];
        for (msg, want) in cases {
            assert_eq!(
                MathError::OrcaQuoteFailed(msg).failure_class(),
                want,
                "{msg}"
            );
        }
    }

    #[test]
    fn revalidation_error_classification_table() {
        let cases = [
            (RevalidationError::NoSnapshot, FailureClass::StaleState),
            (
                RevalidationError::Incoherent { worst_drift: 9 },
                FailureClass::StaleState,
            ),
            (
                RevalidationError::SnapshotTooOld {
                    lag: 9,
                    newest_slot: 9,
                    max: 5,
                },
                FailureClass::StaleState,
            ),
            (
                RevalidationError::CurrentTickArrayMissing { expected_start: 0 },
                FailureClass::MissingTickArray,
            ),
            (
                RevalidationError::ExecutionArrayMissing { start: 0 },
                FailureClass::MissingTickArray,
            ),
        ];
        for (err, want) in cases {
            assert_eq!(err.failure_class(), want, "{err:?}");
        }
    }

    #[test]
    fn tick_array_error_classification_table() {
        let p = Pubkey::new_unique();
        let cases = vec![
            (
                TickArrayError::TooShort { got: 1, need: 2 },
                FailureClass::InvalidPoolState,
            ),
            (
                TickArrayError::WrongOwner {
                    expected: p,
                    got: p,
                },
                FailureClass::InvalidPoolState,
            ),
            (
                TickArrayError::WrongWhirlpool {
                    expected: p,
                    found: p,
                },
                FailureClass::InvalidPoolState,
            ),
            (
                TickArrayError::InvalidStartTick {
                    got: 1,
                    expected: 0,
                    tick_spacing: 64,
                },
                FailureClass::InvalidPoolState,
            ),
            (
                TickArrayError::TickDecode {
                    index: 0,
                    source: anyhow::anyhow!("x"),
                },
                FailureClass::InvalidPoolState,
            ),
            (
                TickArrayError::MissingNeighbor { expected: 0 },
                FailureClass::MissingTickArray,
            ),
        ];
        for (err, want) in cases {
            assert_eq!(err.failure_class(), want, "{err:?}");
        }
    }

    #[test]
    fn cpmm_quote_error_classification_table() {
        use CpmmQuoteError::*;
        for (err, want) in [
            (ZeroReserve, FailureClass::InvalidPoolState),
            (InvalidCreatorFeeMode, FailureClass::InvalidPoolState),
            (Token2022Unsupported, FailureClass::InvalidPoolState),
            (SwapDisabled, FailureClass::InvalidPoolState),
            (PoolNotOpen, FailureClass::InvalidPoolState),
            (ReserveFeeUnderflow, FailureClass::ArithmeticError),
            (ArithmeticOverflow, FailureClass::ArithmeticError),
            (OutputTooLarge, FailureClass::ArithmeticError),
            (InsufficientInputAfterFees, FailureClass::ArithmeticError),
        ] {
            assert_eq!(err.failure_class(), want, "{err:?}");
        }
    }

    // --- anyhow chain classification ---

    #[test]
    fn chain_classification_sees_through_context() {
        use anyhow::Context;
        let programs = atomic_tx_programs(orca(), cpmm());
        let failure = simulation_outcome(
            Some(TransactionError::InstructionError(
                5,
                InstructionError::Custom(6005),
            )),
            &programs,
        )
        .unwrap_err();

        let wrapped: anyhow::Error = Err::<(), _>(failure)
            .context("while simulating")
            .context("while handling opportunity")
            .unwrap_err();
        assert_eq!(
            classify_error_chain(&wrapped),
            FailureClass::ExceededSlippage
        );
    }

    #[test]
    fn chain_classification_handles_each_typed_error() {
        let e: anyhow::Error = MathError::OrcaInsufficientTickCoverage.into();
        assert_eq!(classify_error_chain(&e), FailureClass::MissingTickArray);

        let e: anyhow::Error = RevalidationError::NoSnapshot.into();
        assert_eq!(classify_error_chain(&e), FailureClass::StaleState);

        let e: anyhow::Error = TickArrayError::MissingNeighbor { expected: 0 }.into();
        assert_eq!(classify_error_chain(&e), FailureClass::MissingTickArray);

        let e: anyhow::Error = SimulationFailure::Rpc("x".into()).into();
        assert_eq!(classify_error_chain(&e), FailureClass::RpcError);
    }

    #[test]
    fn client_error_in_chain_is_rpc_error() {
        use solana_client::client_error::{ClientError, ClientErrorKind};
        let client_err = ClientError::from(ClientErrorKind::Custom("boom".into()));
        let e = anyhow::Error::new(client_err).context("Failed to fetch recent blockhash");
        assert_eq!(classify_error_chain(&e), FailureClass::RpcError);
    }

    #[test]
    fn unrecognised_error_defaults_to_execution_error() {
        let e = anyhow::anyhow!("something we have no type for");
        assert_eq!(classify_error_chain(&e), FailureClass::ExecutionError);
    }
}

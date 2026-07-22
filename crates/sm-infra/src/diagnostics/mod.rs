#[cfg(any(test, feature = "test-support"))]
pub mod qsv_ledger;

#[cfg(not(any(test, feature = "test-support")))]
#[expect(
    dead_code,
    reason = "Ledger APIs remain unintegrated until sender wiring"
)]
pub(crate) mod qsv_ledger;

pub use qsv_ledger::TransportLedgerProbe;

use std::sync::Arc;

use sm_domain::transport::{TransportConfig, VideoSender};
use sm_infra::diagnostics::qsv_ledger::TransportLedgerProbe;
use sm_infra::transport::Str0mVideoSender;

#[test]
fn test_support_exposes_only_the_probe_attachment_facade() {
    let sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
    let probe = Arc::new(TransportLedgerProbe::collecting());

    sender.install_transport_ledger_probe_for_test(probe);
}

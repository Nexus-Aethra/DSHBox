//! wsl_vm_delete_tests for dispatch.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use super::record_requests_wsl_vm;

    #[test]
    fn every_vm_spelling_that_starts_a_wsl_distro_is_deleted_too() {
        for spelling in [
            "vm", "VM", "vm:wsl2", "VM:WSL2", "vm-wsl2", "vm_wsl2", "Vm-Wsl2", "vm:",
        ] {
            let value = serde_json::json!({ "runtime": spelling });
            assert!(record_requests_wsl_vm(&value), "missed {spelling:?}");
        }
    }

    #[test]
    fn host_and_other_backends_are_not_wsl_vms() {
        for spelling in ["host", "HOST", "vm:libkrun", "vm:firecracker"] {
            let value = serde_json::json!({ "runtime": spelling });
            assert!(!record_requests_wsl_vm(&value), "wrongly matched {spelling:?}");
        }
        assert!(!record_requests_wsl_vm(&serde_json::json!({})));
        assert!(!record_requests_wsl_vm(&serde_json::json!({ "runtime": null })));
        assert!(!record_requests_wsl_vm(&serde_json::json!({ "runtime": 7 })));
        assert!(!record_requests_wsl_vm(&serde_json::json!({ "runtime": "nonsense" })));
    }

//! package_name_tests for extensions.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use super::is_safe_package_name;

    #[test]
    fn accepts_scoped_and_unscoped_names() {
        // A scoped name is the ordinary case: rejecting `@scope/name` means a
        // container can never have a plugin removed from it.
        assert!(is_safe_package_name("@nexus-aethra/dsh-box-sandbox"));
        assert!(is_safe_package_name("@deepseek-ai/dsh-base"));
        assert!(is_safe_package_name("dsh-box-sandbox"));
        assert!(is_safe_package_name("my.plugin_v2"));
    }

    #[test]
    fn rejects_traversal_and_odd_shapes() {
        assert!(!is_safe_package_name(""));
        assert!(!is_safe_package_name("../etc"));
        assert!(!is_safe_package_name("@scope/../etc"));
        assert!(!is_safe_package_name("@scope"));
        assert!(!is_safe_package_name("a/b/c"));
        assert!(!is_safe_package_name("@/name"));
        assert!(!is_safe_package_name("@sc@ope/name"));
        assert!(!is_safe_package_name("@scope/na@me"));
        assert!(!is_safe_package_name("@scope\\name"));
    }

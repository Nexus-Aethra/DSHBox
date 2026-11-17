//! tests for lockfile.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    /// Shapes taken from a real sealed template's lock: a plain library with no
    /// peers, a plugin with dsh/cordis peers, and a direct dependency whose
    /// version carries pnpm's peer suffix.
    const FIXTURE: &str = r#"
lockfileVersion: '9.0'

importers:

  .:
    dependencies:
      '@deepseek-ai/dsh-ssh':
        specifier: 0.1.6-alpha.1
        version: 0.1.6-alpha.1
      '@nexus-aethra/dshell-bundle':
        specifier: 0.1.5
        version: 0.1.5(@deepseek-ai/dsh-ssh@0.1.6-alpha.1)(zod@4.4.3)

packages:

  '@deepseek-ai/dsh-ssh@0.1.6-alpha.1':
    resolution: {integrity: sha512-aaa}
    peerDependencies:
      '@deepseek-ai/cordis': ^4.0.2
      '@deepseek-ai/dsh-fs': ^0.1.6-alpha.1
  '@nexus-aethra/dshell-conversation@0.1.3':
    resolution: {integrity: sha512-bbb}
    peerDependencies:
      '@deepseek-ai/cordis': ^4.0.2
  zod@4.4.3:
    resolution: {integrity: sha512-ccc}

snapshots:

  '@deepseek-ai/dsh-ssh@0.1.6-alpha.1':
    dependencies:
      '@deepseek-ai/schemastery': 3.18.2
      zod: 4.4.3
  '@nexus-aethra/dshell-bundle@0.1.5':
    dependencies:
      '@nexus-aethra/dshell-conversation': 0.1.3
  '@nexus-aethra/dshell-conversation@0.1.3': {}
  zod@4.4.3: {}
"#;

    #[test]
    fn resolves_the_plugins_and_their_versions() {
        let packages = parse_lockfile(FIXTURE).unwrap();
        let ssh = packages.iter().find(|package| package.name == "@deepseek-ai/dsh-ssh").unwrap();
        assert_eq!(ssh.version, "0.1.6-alpha.1");
        assert!(ssh.direct);
        assert!(ssh.plugin, "a dsh peer makes it a plugin");
        assert_eq!(ssh.specifier.as_deref(), Some("0.1.6-alpha.1"));

        // pnpm's peer suffix is not part of the version.
        let bundle = packages
            .iter()
            .find(|package| package.name == "@nexus-aethra/dshell-bundle")
            .unwrap();
        assert_eq!(bundle.version, "0.1.5");
        assert_eq!(bundle.dependencies.len(), 1);
        assert_eq!(bundle.dependencies[0].name, "@nexus-aethra/dshell-conversation");

        // A plugin nobody's boxfile named, pulled in transitively, is still a
        // plugin: that is the point of reading the lock.
        let conversation = packages
            .iter()
            .find(|package| package.name == "@nexus-aethra/dshell-conversation")
            .unwrap();
        assert!(!conversation.direct);
        assert!(conversation.plugin);

        // A plain library stays a library.
        let zod = packages.iter().find(|package| package.name == "zod").unwrap();
        assert!(!zod.plugin);
        assert!(!zod.direct);

        assert_eq!(plugins(&packages).len(), 3);
        assert!(summary(&packages).contains("4 package(s)"));
    }

    #[test]
    fn lockfile_v6_keys_and_links_are_understood() {
        let v6 = r#"
lockfileVersion: '6.0'
importers:
  .:
    dependencies:
      '@scope/plugin':
        specifier: 1.0.0
        version: 1.0.0
      local-plugin:
        specifier: link:../local-plugin
        version: link:../local-plugin
packages:
  /@scope/plugin@1.0.0:
    resolution: {integrity: sha512-ddd}
    dependencies:
      zod: 4.4.3
  /zod@4.4.3:
    resolution: {integrity: sha512-eee}
"#;
        let packages = parse_lockfile(v6).unwrap();
        let plugin = packages.iter().find(|package| package.name == "@scope/plugin").unwrap();
        assert_eq!(plugin.version, "1.0.0");
        assert_eq!(plugin.dependencies[0].name, "zod");
        // A linked local package has no registry version, and must still show up
        // as a direct dependency rather than vanishing.
        let linked = packages.iter().find(|package| package.name == "local-plugin").unwrap();
        assert!(linked.direct);
        // The raw spec is kept: `link:../local-plugin` says *where* it comes
        // from, which is the useful part for something pnpm never resolved.
        assert_eq!(linked.version, "link:../local-plugin");
    }

    /// A `file:` dependency is keyed by its spec, and the entry under that key
    /// carries the version the package declares. Reporting the spec as the
    /// version would put a relative path where a version belongs.
    #[test]
    fn a_file_dependency_reports_the_version_it_declares() {
        let lock = r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      '@scope/plugin':
        specifier: file:/tmp/pack/plugin-0.1.5.tgz
        version: file:../../../../tmp/pack/plugin-0.1.5.tgz
packages:
  '@scope/plugin@file:../../../../tmp/pack/plugin-0.1.5.tgz':
    resolution: {integrity: sha512-fff, tarball: file:../../../../tmp/pack/plugin-0.1.5.tgz}
    version: 0.1.5
    peerDependencies:
      '@deepseek-ai/cordis': ^4.0.2

snapshots:
  '@scope/plugin@file:../../../../tmp/pack/plugin-0.1.5.tgz(zod@4.4.3)':
    dependencies:
      zod: 4.4.3
"#;
        let packages = parse_lockfile(lock).unwrap();
        let plugin = packages.iter().find(|package| package.name == "@scope/plugin").unwrap();
        assert_eq!(plugin.version, "0.1.5");
        assert!(plugin.direct && plugin.plugin);
        assert_eq!(plugin.specifier.as_deref(), Some("file:/tmp/pack/plugin-0.1.5.tgz"));
        // The `snapshots` entry for the same key carries the edges, not a
        // second copy of the package.
        assert_eq!(packages.len(), 1);
        assert_eq!(plugin.dependencies.len(), 1);
    }

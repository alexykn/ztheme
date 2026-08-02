#[cfg(test)]
use std::ffi::OsStr;
#[cfg(test)]
use std::os::unix::ffi::OsStrExt as _;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Debug)]
pub(crate) struct HashBuilder {
    state: u64,
}

impl HashBuilder {
    pub(crate) fn new(namespace: &[u8]) -> Self {
        let mut hash = Self { state: FNV_OFFSET };
        hash.add_bytes(b"domain", namespace);
        hash
    }

    pub(crate) fn add_bytes(&mut self, label: &[u8], value: &[u8]) {
        self.add_raw(&length(label).to_be_bytes());
        self.add_raw(label);
        self.add_raw(&length(value).to_be_bytes());
        self.add_raw(value);
    }

    #[cfg(test)]
    fn add_os(&mut self, label: &[u8], value: &OsStr) {
        self.add_bytes(label, value.as_bytes());
    }

    #[cfg(test)]
    fn add_optional_os(&mut self, label: &[u8], value: Option<&OsStr>) {
        match value {
            Some(value) => {
                self.add_bytes(b"present", b"1");
                self.add_os(label, value);
            }
            None => self.add_bytes(b"present", b"0"),
        }
    }

    pub(crate) const fn finish(self) -> u64 {
        self.state
    }

    fn add_raw(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(FNV_PRIME);
        }
    }
}

fn length(value: &[u8]) -> u64 {
    u64::try_from(value.len()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::HashBuilder;

    #[test]
    fn outputs_are_stable() {
        assert_eq!(HashBuilder::new(b"one").finish(), 0x9020_a6e7_aadb_4182);

        let mut hash = HashBuilder::new(b"one");
        hash.add_bytes(b"a", b"bc");
        assert_eq!(hash.finish(), 0x1585_6e18_310c_25b9);
    }

    #[test]
    fn namespaces_labels_boundaries_and_optional_values_are_distinct() {
        let mut first = HashBuilder::new(b"one");
        first.add_bytes(b"a", b"bc");
        let mut second = HashBuilder::new(b"one");
        second.add_bytes(b"ab", b"c");
        let mut other_namespace = HashBuilder::new(b"two");
        other_namespace.add_bytes(b"a", b"bc");
        let mut missing = HashBuilder::new(b"optional");
        missing.add_optional_os(b"value", None);
        let mut present = HashBuilder::new(b"optional");
        present.add_optional_os(b"value", Some(OsStr::new("")));

        let first = first.finish();
        assert_ne!(first, second.finish());
        assert_ne!(first, other_namespace.finish());
        assert_ne!(missing.finish(), present.finish());
    }
}

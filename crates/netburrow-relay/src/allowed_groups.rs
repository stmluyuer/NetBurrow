use netburrow_protocol::Group;
use std::{collections::HashSet, fmt, fs, io, path::Path, sync::Arc};

/// Loaded once at startup. Debug output intentionally excludes credentials.
#[derive(Clone, Default)]
pub struct AllowedGroups(Arc<HashSet<Group>>);

impl fmt::Debug for AllowedGroups {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AllowedGroups")
            .field("count", &self.len())
            .finish()
    }
}

impl AllowedGroups {
    pub fn from_file(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot read allowed-groups file: {error}"),
            )
        })?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> io::Result<Self> {
        let mut groups = HashSet::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let group = parse_group_code(line).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid allowed group at line {}", index + 1),
                )
            })?;
            groups.insert(group);
        }
        if groups.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "allowed-groups file contains no groups",
            ));
        }
        Ok(Self(Arc::new(groups)))
    }

    pub fn contains(&self, group: &Group) -> bool {
        self.0.contains(group)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn comments_whitespace_case_and_duplicates() {
        let upper = format!("NB1-{}", "AB".repeat(32));
        let lower = format!("NB1-{}", "ab".repeat(32));
        let groups =
            AllowedGroups::parse(&format!("\n # friends\r\n {upper} \n{lower}\n")).unwrap();
        assert_eq!(groups.len(), 1);
        assert!(groups.contains(&[0xab; 32]));
        assert!(!format!("{groups:?}").contains(&upper));
    }
    #[test]
    fn empty_invalid_and_zero_fail_without_exposing_entry() {
        for text in ["", " \n# none\n"] {
            assert!(AllowedGroups::parse(text).is_err());
        }
        for entry in [
            "secret bad input".to_owned(),
            format!("NB1-{}", "0".repeat(64)),
            format!("NB1-{}", "zz".repeat(32)),
        ] {
            let error = AllowedGroups::parse(&format!("# header\n{entry}"))
                .unwrap_err()
                .to_string();
            assert_eq!(error, "invalid allowed group at line 2");
            assert!(!error.contains(&entry));
        }
    }
    #[test]
    fn missing_file_fails() {
        assert!(
            AllowedGroups::from_file(Path::new("/nonexistent/netburrow-allowed-groups-test"))
                .is_err()
        );
    }
}

fn parse_group_code(input: &str) -> Result<Group, String> {
    let input = input.trim();
    let hex = input
        .strip_prefix("NB1-")
        .ok_or("联机组格式无效，请创建或导入 NB1- 开头的组码")?;
    if hex.len() != 64 {
        return Err(
            "联机组不完整：应为 NB1- 加 64 位十六进制字符，请重新复制朋友的完整组码".into(),
        );
    }
    let mut group = [0; 32];
    for (slot, pair) in group.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let digit = |b: u8| {
            (b as char)
                .to_digit(16)
                .map(|n| n as u8)
                .ok_or("联机组含无效字符，请重新复制朋友的完整组码，或创建新组并分享")
        };
        *slot = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    if group == [0; 32] {
        return Err("全零组码无效，请重新创建联机组".into());
    }
    Ok(group)
}

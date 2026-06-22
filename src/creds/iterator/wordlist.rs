use std::{
    fs::File,
    io::Read,
};

use crate::{creds, session::Error};

const BOM_UTF8: &[u8] = &[0xEF, 0xBB, 0xBF];

fn read_lines(path: &str) -> Result<Vec<String>, Error> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;

    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).map_err(|e| e.to_string())?;

    if buffer.starts_with(BOM_UTF8) {
        buffer = buffer[BOM_UTF8.len()..].to_vec();
    }

    let content = String::from_utf8(buffer).map_err(|e| e.to_string())?;
    let mut lines = Vec::new();

    for line in content.lines() {
        let line = line.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        lines.push(line.to_owned());
    }

    Ok(lines)
}

pub(crate) struct Wordlist {
    path: String,
    lines: Vec<String>,
    current: usize,
    elements: usize,
}

impl Wordlist {
    pub fn new(path: String) -> Result<Self, Error> {
        log::debug!("loading wordlist from {} ...", &path);

        let lines = read_lines(&path)?;
        let elements = lines.len();

        Ok(Self {
            path,
            elements,
            current: 0,
            lines,
        })
    }
}

impl creds::Iterator for Wordlist {
    fn search_space_size(&self) -> usize {
        self.elements
    }
}

impl creds::IteratorClone for Wordlist {
    fn create_boxed_copy(&self) -> Box<dyn creds::Iterator> {
        Box::new(Self::new(self.path.clone()).unwrap())
    }
}

impl std::iter::Iterator for Wordlist {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current < self.elements {
            let line = self.lines[self.current].clone();
            self.current += 1;
            Some(line)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;

    use crate::creds::{Expression, iterator};

    #[test]
    fn can_handle_wordlist() {
        let num_items = 3;
        let mut expected = vec![];
        let tmpdir = tempfile::tempdir().unwrap();
        let tmppath = tmpdir.path().join("wordlist.txt");
        let mut tmpwordlist = File::create(&tmppath).unwrap();

        for i in 0..num_items {
            writeln!(tmpwordlist, "item{}", i).unwrap();
            expected.push(format!("item{}", i));
        }
        tmpwordlist.flush().unwrap();
        drop(tmpwordlist);

        let iter = iterator::new(Expression::Wordlist {
            filename: tmppath.to_str().unwrap().to_owned(),
        })
        .unwrap();
        let tot = iter.search_space_size();
        let vec: Vec<String> = iter.collect();

        assert_eq!(tot, num_items);
        assert_eq!(vec, expected);
    }

    #[test]
    fn wordlist_skips_empty_lines_and_comments() {
        let tmpdir = tempfile::tempdir().unwrap();
        let tmppath = tmpdir.path().join("wordlist.txt");
        let mut tmpwordlist = File::create(&tmppath).unwrap();

        writeln!(tmpwordlist, "# this is a comment").unwrap();
        writeln!(tmpwordlist, "valid1").unwrap();
        writeln!(tmpwordlist, "").unwrap();
        writeln!(tmpwordlist, "   ").unwrap();
        writeln!(tmpwordlist, "# another comment").unwrap();
        writeln!(tmpwordlist, "valid2").unwrap();
        writeln!(tmpwordlist, "valid3").unwrap();
        tmpwordlist.flush().unwrap();
        drop(tmpwordlist);

        let iter = iterator::new(Expression::Wordlist {
            filename: tmppath.to_str().unwrap().to_owned(),
        })
        .unwrap();
        let vec: Vec<String> = iter.collect();

        assert_eq!(vec, vec!["valid1", "valid2", "valid3"]);
    }

    #[test]
    fn wordlist_handles_bom() {
        let tmpdir = tempfile::tempdir().unwrap();
        let tmppath = tmpdir.path().join("wordlist.txt");
        let mut tmpwordlist = File::create(&tmppath).unwrap();

        let bom: &[u8] = &[0xEF, 0xBB, 0xBF];
        tmpwordlist.write_all(bom).unwrap();
        writeln!(tmpwordlist, "item1").unwrap();
        writeln!(tmpwordlist, "item2").unwrap();
        tmpwordlist.flush().unwrap();
        drop(tmpwordlist);

        let iter = iterator::new(Expression::Wordlist {
            filename: tmppath.to_str().unwrap().to_owned(),
        })
        .unwrap();
        let vec: Vec<String> = iter.collect();

        assert_eq!(vec, vec!["item1", "item2"]);
    }

    #[test]
    fn wordlist_handles_crlf_and_carriage_return() {
        let tmpdir = tempfile::tempdir().unwrap();
        let tmppath = tmpdir.path().join("wordlist.txt");
        let mut tmpwordlist = File::create(&tmppath).unwrap();

        tmpwordlist.write_all(b"line1\r\n").unwrap();
        tmpwordlist.write_all(b"line2\r\n").unwrap();
        tmpwordlist.write_all(b"line3\r").unwrap();
        tmpwordlist.flush().unwrap();
        drop(tmpwordlist);

        let iter = iterator::new(Expression::Wordlist {
            filename: tmppath.to_str().unwrap().to_owned(),
        })
        .unwrap();
        let vec: Vec<String> = iter.collect();

        assert_eq!(vec, vec!["line1", "line2", "line3"]);
    }
}

use std::io::{self, Write};

const MAGIC: &str = "ZTHEME1";

pub fn prompt_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for character in value.chars() {
        match character {
            '%' => escaped.push_str("%%"),
            '\t' | '\r' | '\n' => escaped.push(' '),
            character if character.is_control() => escaped.push('?'),
            character => escaped.push(character),
        }
    }

    escaped
}

pub fn write_segment(
    output: &mut impl Write,
    generation: u64,
    segment: &str,
    fragment: &str,
) -> io::Result<()> {
    write_fields(output, generation, &["segment", segment, fragment])
}

pub fn write_error(
    output: &mut impl Write,
    generation: u64,
    segment: &str,
    message: &str,
) -> io::Result<()> {
    let message = prompt_text(message);
    write_fields(output, generation, &["error", segment, &message])
}

pub fn write_done(output: &mut impl Write, generation: u64) -> io::Result<()> {
    write_fields(output, generation, &["done"])
}

fn write_fields(output: &mut impl Write, generation: u64, fields: &[&str]) -> io::Result<()> {
    write!(output, "{MAGIC}\t{generation}")?;
    for field in fields {
        write!(output, "\t{field}")?;
    }
    writeln!(output)?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::{prompt_text, write_done, write_error, write_segment};

    #[test]
    fn prompt_text_escapes_prompt_sequences_and_controls() {
        assert_eq!(prompt_text("100%\tready\n\u{1b}"), "100%% ready ?");
        assert_eq!(prompt_text("Grüße 🚀"), "Grüße 🚀");
    }

    #[test]
    fn records_have_exact_tab_delimited_framing() {
        let mut output = Vec::new();
        write_segment(&mut output, 7, "git", " main").unwrap();
        write_error(&mut output, 7, "runtime", "bad\tvalue\nnext").unwrap();
        write_done(&mut output, 7).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "ZTHEME1\t7\tsegment\tgit\t main\n\
             ZTHEME1\t7\terror\truntime\tbad value next\n\
             ZTHEME1\t7\tdone\n"
        );
    }
}

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

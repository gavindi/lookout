/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::marker::PhantomData;
use std::ops::{RangeFrom, RangeInclusive};
use std::str;

use crate::types::{AttrMacro, Attribute, State};

pub struct CommandBuilder {}

impl CommandBuilder {
    pub fn check() -> Command {
        let args = b"CHECK".to_vec();
        Command {
            args,
            next_state: None,
        }
    }

    pub fn close() -> Command {
        let args = b"CLOSE".to_vec();
        Command {
            args,
            next_state: Some(State::Authenticated),
        }
    }

    pub fn examine(mailbox: &str) -> SelectCommand<select::NoParams> {
        let mut args = b"EXAMINE \"".to_vec();
        push_quoted(&mut args, mailbox).expect("mailbox name must not contain CR or LF");
        args.push(b'"');
        SelectCommand {
            args,
            state: PhantomData,
        }
    }

    pub fn fetch() -> FetchCommand<fetch::Empty> {
        FetchCommand {
            args: b"FETCH ".to_vec(),
            state: PhantomData,
        }
    }

    pub fn list(reference: &str, glob: &str) -> Command {
        let mut args = b"LIST \"".to_vec();
        push_quoted(&mut args, reference).expect("reference must not contain CR or LF");
        args.extend(b"\" \"");
        push_quoted(&mut args, glob).expect("glob must not contain CR or LF");
        args.push(b'"');
        Command {
            args,
            next_state: None,
        }
    }

    pub fn login(user_name: &str, password: &str) -> Command {
        let mut args = b"LOGIN \"".to_vec();
        push_quoted(&mut args, user_name).expect("user name must not contain CR or LF");
        args.extend(b"\" \"");
        push_quoted(&mut args, password).expect("password must not contain CR or LF");
        args.push(b'"');
        Command {
            args,
            next_state: Some(State::Authenticated),
        }
    }

    pub fn select(mailbox: &str) -> SelectCommand<select::NoParams> {
        let mut args = b"SELECT \"".to_vec();
        push_quoted(&mut args, mailbox).expect("mailbox name must not contain CR or LF");
        args.push(b'"');
        SelectCommand {
            args,
            state: PhantomData,
        }
    }

    pub fn uid_fetch() -> FetchCommand<fetch::Empty> {
        FetchCommand {
            args: b"UID FETCH ".to_vec(),
            state: PhantomData,
        }
    }
}

pub struct Command {
    pub args: Vec<u8>,
    pub next_state: Option<State>,
}

pub struct SelectCommand<T> {
    args: Vec<u8>,
    state: PhantomData<T>,
}

impl SelectCommand<select::NoParams> {
    // RFC 4551 CONDSTORE parameter (based on RFC 4466 `select-param`)
    pub fn cond_store(mut self) -> SelectCommand<select::Params> {
        self.args.extend(b" (CONDSTORE");
        SelectCommand {
            args: self.args,
            state: PhantomData,
        }
    }
}

impl From<SelectCommand<select::NoParams>> for Command {
    fn from(cmd: SelectCommand<select::NoParams>) -> Command {
        Command {
            args: cmd.args,
            next_state: Some(State::Selected),
        }
    }
}

impl From<SelectCommand<select::Params>> for Command {
    fn from(mut cmd: SelectCommand<select::Params>) -> Command {
        cmd.args.push(b')');
        Command {
            args: cmd.args,
            next_state: Some(State::Selected),
        }
    }
}

pub mod select {
    pub struct NoParams;
    pub struct Params;
}

pub mod fetch {
    pub struct Empty;
    pub struct Messages;
    pub struct Attributes;
    pub struct Modifiers;
}

pub struct FetchCommand<T> {
    args: Vec<u8>,
    state: PhantomData<T>,
}

impl FetchCommand<fetch::Empty> {
    pub fn num(mut self, num: u32) -> FetchCommand<fetch::Messages> {
        sequence_num(&mut self.args, num);
        FetchCommand {
            args: self.args,
            state: PhantomData,
        }
    }

    pub fn range(mut self, range: RangeInclusive<u32>) -> FetchCommand<fetch::Messages> {
        sequence_range(&mut self.args, range);
        FetchCommand {
            args: self.args,
            state: PhantomData,
        }
    }

    pub fn range_from(mut self, range: RangeFrom<u32>) -> FetchCommand<fetch::Messages> {
        range_from(&mut self.args, range);
        FetchCommand {
            args: self.args,
            state: PhantomData,
        }
    }
}

impl FetchCommand<fetch::Messages> {
    pub fn num(mut self, num: u32) -> FetchCommand<fetch::Messages> {
        self.args.extend(b",");
        sequence_num(&mut self.args, num);
        self
    }

    pub fn range(mut self, range: RangeInclusive<u32>) -> FetchCommand<fetch::Messages> {
        self.args.extend(b",");
        sequence_range(&mut self.args, range);
        self
    }

    pub fn range_from(mut self, range: RangeFrom<u32>) -> FetchCommand<fetch::Messages> {
        self.args.extend(b",");
        range_from(&mut self.args, range);
        self
    }

    pub fn attr_macro(mut self, named: AttrMacro) -> FetchCommand<fetch::Modifiers> {
        self.args.push(b' ');
        self.args.extend(
            match named {
                AttrMacro::All => "ALL",
                AttrMacro::Fast => "FAST",
                AttrMacro::Full => "FULL",
            }
            .as_bytes(),
        );
        FetchCommand {
            args: self.args,
            state: PhantomData,
        }
    }

    pub fn attr(mut self, attr: Attribute) -> FetchCommand<fetch::Attributes> {
        self.args.extend(b" (");
        push_attr(&mut self.args, attr);
        FetchCommand {
            args: self.args,
            state: PhantomData,
        }
    }
}

fn sequence_num(cmd: &mut Vec<u8>, num: u32) {
    push_decimal(cmd, num);
}

fn sequence_range(cmd: &mut Vec<u8>, range: RangeInclusive<u32>) {
    push_decimal(cmd, *range.start());
    cmd.push(b':');
    push_decimal(cmd, *range.end());
}

fn range_from(cmd: &mut Vec<u8>, range: RangeFrom<u32>) {
    push_decimal(cmd, range.start);
    cmd.extend(b":*");
}

impl FetchCommand<fetch::Attributes> {
    pub fn attr(mut self, attr: Attribute) -> FetchCommand<fetch::Attributes> {
        self.args.push(b' ');
        push_attr(&mut self.args, attr);
        self
    }

    pub fn changed_since(mut self, seq: u64) -> FetchCommand<fetch::Modifiers> {
        self.args.push(b')');
        changed_since(&mut self.args, seq);
        FetchCommand {
            args: self.args,
            state: PhantomData,
        }
    }
}

fn push_attr(cmd: &mut Vec<u8>, attr: Attribute) {
    cmd.extend(
        match attr {
            Attribute::Body => "BODY",
            Attribute::Envelope => "ENVELOPE",
            Attribute::Flags => "FLAGS",
            Attribute::InternalDate => "INTERNALDATE",
            Attribute::ModSeq => "MODSEQ",
            Attribute::Rfc822 => "RFC822",
            Attribute::Rfc822Size => "RFC822.SIZE",
            Attribute::Rfc822Text => "RFC822.TEXT",
            Attribute::Uid => "UID",
            Attribute::GmailLabels => "X-GM-LABELS",
            Attribute::GmailMsgId => "X-GM-MSGID",
            Attribute::GmailThrId => "X-GM-THRID",
        }
        .as_bytes(),
    );
}

impl From<FetchCommand<fetch::Attributes>> for Command {
    fn from(mut cmd: FetchCommand<fetch::Attributes>) -> Command {
        cmd.args.push(b')');
        Command {
            args: cmd.args,
            next_state: None,
        }
    }
}

impl From<FetchCommand<fetch::Modifiers>> for Command {
    fn from(cmd: FetchCommand<fetch::Modifiers>) -> Command {
        Command {
            args: cmd.args,
            next_state: None,
        }
    }
}

impl FetchCommand<fetch::Modifiers> {
    pub fn changed_since(mut self, seq: u64) -> FetchCommand<fetch::Modifiers> {
        changed_since(&mut self.args, seq);
        self
    }
}

fn changed_since(cmd: &mut Vec<u8>, seq: u64) {
    cmd.extend(b" (CHANGEDSINCE ");
    push_decimal(cmd, seq);
    cmd.push(b')');
}

/// Appends `num`'s decimal representation to `cmd` without a temporary
/// `String` allocation (itoa-style). Builders call this once per sequence
/// number in a joined UID/sequence set, where a folder's whole mailbox can
/// mean thousands of numbers in one command line.
fn push_decimal<T: Into<u64>>(cmd: &mut Vec<u8>, num: T) {
    // Longest form is 20 digits (u64::MAX); writing backwards into the stack
    // buffer avoids the reverse afterwards.
    let mut num = num.into();
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (num % 10) as u8;
        num /= 10;
        if num == 0 {
            break;
        }
    }
    cmd.extend_from_slice(&buf[i..]);
}

/// Appends `s` to `cmd`, escaped for use as the *body* of a quoted string per
/// the IMAPv4 RFC (the surrounding DQUOTEs are the caller's). Returns an
/// error if the argument contains characters a quoted string can't carry.
///
/// Relevant definitions from RFC 3501 formal syntax:
///
/// string = quoted / literal [literal elided here]
/// quoted = DQUOTE *QUOTED-CHAR DQUOTE
/// QUOTED-CHAR = <any TEXT-CHAR except quoted-specials> / "\" quoted-specials
/// quoted-specials = DQUOTE / "\"
/// TEXT-CHAR = <any CHAR except CR and LF>
fn push_quoted(cmd: &mut Vec<u8>, s: &str) -> Result<(), &'static str> {
    for b in s.bytes() {
        match b {
            b'\r' | b'\n' => {
                return Err("CR and LF not allowed in quoted strings");
            }
            b'\\' | b'"' => {
                cmd.push(b'\\');
                cmd.push(b);
            }
            _ => cmd.push(b),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{push_quoted, Attribute, Command, CommandBuilder};

    #[test]
    fn login() {
        assert_eq!(
            CommandBuilder::login("djc", "s3cr3t").args,
            b"LOGIN \"djc\" \"s3cr3t\""
        );
        assert_eq!(
            CommandBuilder::login("djc", "domain\\password").args,
            b"LOGIN \"djc\" \"domain\\\\password\""
        );
    }

    #[test]
    fn select() {
        let cmd = Command::from(CommandBuilder::select("INBOX"));
        assert_eq!(&cmd.args, br#"SELECT "INBOX""#);
        let cmd = Command::from(CommandBuilder::examine("INBOX").cond_store());
        assert_eq!(&cmd.args, br#"EXAMINE "INBOX" (CONDSTORE)"#);
    }

    #[test]
    fn fetch() {
        let cmd: Command = CommandBuilder::fetch()
            .range_from(1..)
            .attr(Attribute::Uid)
            .attr(Attribute::ModSeq)
            .changed_since(13)
            .into();
        assert_eq!(cmd.args, &b"FETCH 1:* (UID MODSEQ) (CHANGEDSINCE 13)"[..]);

        let cmd: Command = CommandBuilder::fetch()
            .num(1)
            .num(2)
            .attr(Attribute::Uid)
            .attr(Attribute::ModSeq)
            .into();
        assert_eq!(cmd.args, &b"FETCH 1,2 (UID MODSEQ)"[..]);
    }

    #[test]
    fn test_quoted_string() {
        let mut buf = Vec::new();
        push_quoted(&mut buf, "a").unwrap();
        assert_eq!(buf, b"a");
        buf.clear();
        push_quoted(&mut buf, "").unwrap();
        assert_eq!(buf, b"");
        buf.clear();
        push_quoted(&mut buf, "a\"b\\c").unwrap();
        assert_eq!(buf, br#"a\"b\\c"#);
        buf.clear();
        push_quoted(&mut buf, "\"foo\\").unwrap();
        assert_eq!(buf, br#"\"foo\\"#);
        buf.clear();
        assert!(push_quoted(&mut buf, "\n").is_err());
    }
}

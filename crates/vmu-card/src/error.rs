use core::fmt;

/// Everything that can go wrong reading or changing a card or a save file.
///
/// Every check that guards a write happens before the first block is planned, so an
/// error always means the card was left as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// An image that is not 128 KiB.
    WrongSize(usize),
    /// The root block does not open with the sixteen `0x55` bytes of a formatted card.
    NotFormatted,
    /// A root-block field points somewhere a standard card cannot.
    BadLayout(&'static str),
    /// A FAT chain that loops, leaves the card, or ends early.
    BadChain { name: String, block: u16 },
    /// A save whose name is already on the card. Imports only add.
    NameTaken(String),
    /// Every directory slot is in use (208 of them, so in practice space runs out first).
    DirectoryFull,
    /// Not enough free blocks.
    NoSpace { needed: usize, free: usize },
    /// A mini-game needs blocks 0 upward free, and only one fits on a card.
    GameAreaBusy,
    /// A save file with no data, or larger than a card.
    BadSize(usize),
    /// A `.VMI` that cannot be read.
    BadVmi(&'static str),
    /// A `.DCI` that cannot be read.
    BadDci(&'static str),
    /// A card filename that is empty or not 12 bytes of printable ASCII.
    BadName,
    /// A timestamp that is not a real date.
    BadTime,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongSize(n) => write!(f, "image is {n} bytes, not 131072"),
            Self::NotFormatted => write!(f, "card is not formatted"),
            Self::BadLayout(what) => write!(f, "root block: {what}"),
            Self::BadChain { name, block } => {
                write!(f, "{name}: broken block chain at block {block}")
            }
            Self::NameTaken(name) => write!(f, "{name} is already on the card"),
            Self::DirectoryFull => write!(f, "the card's directory is full"),
            Self::NoSpace { needed, free } => {
                write!(f, "needs {needed} blocks, {free} free")
            }
            Self::GameAreaBusy => write!(f, "a mini-game needs the start of the card free"),
            Self::BadSize(n) => write!(f, "save of {n} bytes does not fit a card"),
            Self::BadVmi(what) => write!(f, "VMI: {what}"),
            Self::BadDci(what) => write!(f, "DCI: {what}"),
            Self::BadName => write!(f, "card filename must be 1-12 printable ASCII bytes"),
            Self::BadTime => write!(f, "not a valid date and time"),
        }
    }
}

impl std::error::Error for Error {}

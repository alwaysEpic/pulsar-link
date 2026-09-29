//! Round-trips every save on real card images through every format.
//!
//! Real cards hold someone's saves, so they are never committed. Point the test at
//! them instead (colon-separated), and run it on purpose:
//!
//! ```text
//! VMU_CARD_IMAGES=~/…/vmu_save_A1.bin:pulled.bin cargo test -p vmu-card -- --ignored --nocapture
//! ```

// Inside a cfg(test) module so clippy treats the helpers as test code too.
#[cfg(test)]
mod real {
    use vmu_card::{
        Card, SaveFile, Timestamp, Vmi, dci_from_save, save_from_dci, save_from_vms, stale_blocks,
    };

    fn images() -> Vec<(String, Card)> {
        let paths = std::env::var("VMU_CARD_IMAGES").expect("set VMU_CARD_IMAGES");
        paths
            .split(':')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let bytes = std::fs::read(p).unwrap_or_else(|e| panic!("{p}: {e}"));
                (p.to_owned(), Card::from_image(&bytes).unwrap())
            })
            .collect()
    }

    fn now() -> Timestamp {
        Timestamp::new(2026, 9, 25, 12, 0, 0).unwrap()
    }

    /// What must survive a trip: everything but a date the card could not parse, which
    /// import replaces with `now`.
    fn same(a: &SaveFile, b: &SaveFile) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.copy_protected, b.copy_protected);
        assert_eq!(a.header_block, b.header_block);
        assert_eq!(a.data, b.data, "{}", a.name_str());
        if a.modified.is_some() {
            assert_eq!(a.modified, b.modified);
        }
    }

    #[test]
    #[ignore = "needs VMU_CARD_IMAGES: real cards are not committed"]
    fn every_save_round_trips_through_every_format() {
        for (path, card) in images() {
            let files = card.files().unwrap();
            println!(
                "{path}: {} files, {} blocks free",
                files.len(),
                card.free_blocks().unwrap()
            );
            assert!(stale_blocks(&card, &card).unwrap().is_empty());

            let mut rebuilt = Card::formatted(now());
            for e in &files {
                let save = card.export(e).unwrap();
                let head = save.header();
                let frames = save.icon().map_or(0, |i| i.frames.len());
                println!(
                    "  {:<12} {:?} {:>3} blocks  {:?}  {:?}  icon frames {}",
                    e.name_str(),
                    e.kind,
                    e.size_blocks,
                    e.modified.map(|t| (t.year, t.month, t.day)),
                    head.map(|h| h.dc_description).unwrap_or_default(),
                    frames,
                );

                let vmi = Vmi::for_save(&save, "TEST").unwrap();
                let back = Vmi::parse(&vmi.to_bytes()).unwrap();
                same(&save, &save_from_vms(&save.data, &back).unwrap());
                same(&save, &save_from_dci(&dci_from_save(&save)).unwrap());

                rebuilt.import(&save, now()).unwrap();
            }

            // The rebuilt card holds the same saves, byte for byte.
            let again = rebuilt.files().unwrap();
            assert_eq!(again.len(), files.len());
            for e in &files {
                let r = again.iter().find(|a| a.name == e.name).unwrap();
                same(&card.export(e).unwrap(), &rebuilt.export(r).unwrap());
            }
            assert_eq!(rebuilt.free_blocks().unwrap(), card.free_blocks().unwrap());
        }
    }
}

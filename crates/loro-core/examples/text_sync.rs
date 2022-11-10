#[cfg(not(feature = "fuzzing"))]
fn main() {}

#[cfg(feature = "fuzzing")]
fn main() {
    const RAW_DATA: &[u8; 901823] = include_bytes!("../benches/automerge-paper.json.gz");
    use std::{io::Read, time::Instant};

    use flate2::read::GzDecoder;
    use loro_core::{container::manager::LockContainer, LoroCore};
    use serde_json::Value;

    let mut d = GzDecoder::new(&RAW_DATA[..]);
    let mut s = String::new();
    d.read_to_string(&mut s).unwrap();
    let json: Value = serde_json::from_str(&s).unwrap();
    let txns = json.as_object().unwrap().get("txns");
    println!("Txn: {}", txns.unwrap().as_array().unwrap().len());

    let mut loro = LoroCore::default();
    let mut loro_b = LoroCore::default();
    let mut loro_c = LoroCore::default();
    let start = Instant::now();
    for (i, txn) in txns.unwrap().as_array().unwrap().into_iter().enumerate() {
        let get_or_create_root_text = loro.get_or_create_root_text("text");
        let mut text = get_or_create_root_text.lock_text();
        let patches = txn
            .as_object()
            .unwrap()
            .get("patches")
            .unwrap()
            .as_array()
            .unwrap();
        for patch in patches {
            let pos = patch[0].as_u64().unwrap() as usize;
            let del_here = patch[1].as_u64().unwrap() as usize;
            let ins_content = patch[2].as_str().unwrap();
            text.delete(pos, del_here);
            text.insert(pos, ins_content);
        }

        drop(text);
        let get_or_create_root_text = loro_b.get_or_create_root_text("text");
        let mut text = get_or_create_root_text.lock_text();
        for patch in patches {
            let pos = patch[0].as_u64().unwrap() as usize;
            let del_here = patch[1].as_u64().unwrap() as usize;
            let ins_content = patch[2].as_str().unwrap();
            text.delete(pos, del_here);
            text.insert(pos, ins_content);
        }
        drop(text);
        if i % 10 == 0 {
            loro.import(loro_b.export(loro.vv()));
            loro_b.import(loro.export(loro_b.vv()));
        }
    }
    loro_b.debug_inspect();
    loro.debug_inspect();
    println!("Elapsed {}ms", start.elapsed().as_millis());
    loro_c.import(loro.export(loro_c.vv()));
    println!("Elapsed {}ms", start.elapsed().as_millis());
}

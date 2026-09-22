use ragnarok_formats::lub;
use ragnarok_formats::lua_table;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: dump_lub <path>")?;
    let data = std::fs::read(&path)?;
    println!(
        "size={} is_compiled_chunk={}",
        data.len(),
        lub::is_compiled_chunk(&data)
    );

    let info = match lua_table::parse_item_info_lub(&data) {
        Ok(info) => info,
        Err(e) => {
            println!("parse error: {e:?}");
            if let lub::LubError::SyntaxErrorAt(pos) = e {
                let start = pos.saturating_sub(80);
                let end = (pos + 80).min(data.len());
                println!("--- context [{start}..{end}), error at {pos} ---");
                println!("{}", String::from_utf8_lossy(&data[start..end]));
            }
            return Ok(());
        }
    };
    println!(
        "identified: name={} resource={} description={}",
        info.identified_name.len(),
        info.identified_resource.len(),
        info.identified_description.len()
    );
    println!(
        "unidentified: name={} resource={} description={}",
        info.unidentified_name.len(),
        info.unidentified_resource.len(),
        info.unidentified_description.len()
    );

    let mut ids: Vec<u16> = info.identified_name.keys().copied().collect();
    ids.sort_unstable();

    let query_ids: Vec<u16> = std::env::args()
        .skip(2)
        .filter_map(|a| a.parse().ok())
        .collect();
    let show: Vec<u16> = if query_ids.is_empty() {
        ids.iter()
            .take(3)
            .chain(ids.iter().rev().take(3))
            .copied()
            .collect()
    } else {
        for &id in &query_ids {
            println!("query {id}: present={}", ids.binary_search(&id).is_ok());
        }
        query_ids
    };

    for &id in &show {
        println!("\n== item {id} ==");
        println!("  identifiedDisplayName = {:?}", info.identified_name.get(&id));
        println!(
            "  identifiedResourceName = {:?}",
            info.identified_resource.get(&id)
        );
        println!(
            "  identifiedDescriptionName = {:?}",
            info.identified_description.get(&id)
        );
    }
    Ok(())
}

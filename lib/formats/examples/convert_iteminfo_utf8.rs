use ragnarok_formats::lua_source;
use ragnarok_formats::lua_table;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: convert_iteminfo_utf8 <path to plain-text iteminfo lub/lua>")?;
    let data = std::fs::read(&path)?;

    let before = lua_table::parse_item_info_lub(&data)?;
    println!(
        "before: identified name={} resource={} description={}",
        before.identified_name.len(),
        before.identified_resource.len(),
        before.identified_description.len()
    );

    let converted = lua_source::transcode_to_utf8(&data);
    let out_path = format!("{path}.utf8");
    std::fs::write(&out_path, &converted)?;
    println!("wrote {out_path} ({} bytes)", converted.len());

    let after = lua_table::parse_item_info_lub(&converted)?;
    println!(
        "after:  identified name={} resource={} description={}",
        after.identified_name.len(),
        after.identified_resource.len(),
        after.identified_description.len()
    );

    let mismatches = before
        .identified_name
        .iter()
        .filter(|(id, name)| after.identified_name.get(id) != Some(*name))
        .count()
        + before
            .identified_resource
            .iter()
            .filter(|(id, res)| after.identified_resource.get(id) != Some(*res))
            .count();
    println!("mismatched entries after round-trip: {mismatches}");

    Ok(())
}

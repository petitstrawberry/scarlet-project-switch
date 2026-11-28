// SPDX-License-Identifier: GPL-2.0-only
//! Respect console paths and serial aliases with optional baud/format suffixes.
pub(super) fn exclude_chosen(fdt: &fdt::Fdt<'_>, address: u64) -> Result<(), &'static str> {
    let chosen = fdt
        .find_node("/chosen")
        .ok_or("missing chosen console handoff")?;
    for name in ["stdout-path", "linux,stdout-path", "stdin-path"] {
        let Some(prop) = chosen.property(name) else {
            continue;
        };
        let path = prop
            .as_str()
            .and_then(|path| path.split(':').next())
            .filter(|s| !s.is_empty())
            .ok_or("invalid chosen console path")?;
        // find_node handles /aliases after trimming any serial options.
        let node = fdt
            .find_node(path)
            .ok_or("unresolved chosen console path")?;
        if node
            .reg()
            .and_then(|mut regs| regs.next())
            .is_some_and(|reg| reg.starting_address as u64 == address)
        {
            return Err("Tegra UART reserved for console");
        }
    }
    Ok(())
}

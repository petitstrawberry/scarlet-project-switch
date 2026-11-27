// SPDX-License-Identifier: GPL-2.0-only
use super::Error;
use fdt::node::FdtNode;
fn cell(n: FdtNode<'_, '_>, name: &str, i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        n.property(name)?
            .value
            .get(i * 4..i * 4 + 4)?
            .try_into()
            .ok()?,
    ))
}
fn text<'a>(n: FdtNode<'_, 'a>, name: &str) -> Option<&'a str> {
    n.property(name)?.as_str()
}
fn phandle(n: FdtNode<'_, '_>) -> Result<u32, Error> {
    cell(n, "phandle", 0)
        .filter(|&p| p != 0)
        .ok_or("missing external display phandle")
}
fn enabled(n: FdtNode<'_, '_>) -> bool {
    n.property("status")
        .is_none_or(|p| matches!(p.as_str(), Some("okay" | "ok")))
}
pub(super) fn validate(f: &fdt::Fdt, provider: u32, bases: [u64; 3]) -> Result<u32, Error> {
    let node = |path: &str| {
        f.find_node(path)
            .filter(|n| enabled(*n))
            .ok_or("missing Icosa external display DT node")
    };
    if bases != [0x54240000, 0x54580000, 0x54040000]
        || !f.root().compatible().all().any(|s| s == "nintendo,icosa")
    {
        return Err("unsupported external display route");
    }
    node("/host1x")?;
    let dc = node("/host1x/dc@54240000")?;
    let sor = node("/host1x/sor1")?;
    let aux = node("/host1x/dpaux1")?;
    let panel = node("/host1x/sor1/dp-display")?;
    let out = node("/host1x/sor1/dp-display/disp-default-out")?;
    for ((n, id, compatible, clock_index), base) in [
        (dc, 26, "nvidia,tegra210-dc", 0),
        (sor, 183, "nvidia,tegra210-sor1", 6),
        (aux, 207, "nvidia,tegra210-dpaux1", 0),
    ]
    .into_iter()
    .zip(bases)
    {
        if !n
            .compatible()
            .is_some_and(|c| c.all().any(|s| s == compatible))
            || cell(n, "clocks", clock_index) != Some(provider)
            || cell(n, "clocks", clock_index + 1) != Some(id)
        {
            return Err("incorrect external display clock or compatible");
        }
        if !n
            .reg()
            .and_then(|mut r| r.next())
            .is_some_and(|r| r.starting_address as u64 == base && r.size == Some(0x40000))
            || cell(n, "resets", 0) != Some(provider)
            || cell(n, "resets", 1) != Some(id)
        {
            return Err("incorrect DC1/SOR1/DPAUX1 register or reset wiring");
        }
    }
    const XBAR: [u32; 5] = [2, 1, 0, 3, 4];
    let xbar_matches = sor.property("nvidia,xbar-ctrl").is_some_and(|p| {
        p.value
            .chunks_exact(4)
            .map(|v| u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
            .eq(XBAR)
            && p.value.len() == 4 * XBAR.len()
    });
    if !xbar_matches
        || sor.property("nvidia,typec-port").map(|p| p.value) != Some(&[1])
        || panel.property("nvidia,fast-lt-disable").is_none()
    {
        return Err("incorrect Icosa SOR1 lane routing or training method");
    }
    if cell(dc, "nvidia,dc-ctrlnum", 0) != Some(1)
        || text(dc, "nvidia,dc-or-node") != Some("/host1x/sor1")
        || cell(dc, "nvidia,dc-connector", 0) != Some(phandle(sor)?)
        || cell(sor, "nvidia,sor-ctrlnum", 0) != Some(1)
        || cell(sor, "nvidia,dpaux", 0) != Some(phandle(aux)?)
        || cell(aux, "nvidia,dpaux-ctrlnum", 0) != Some(1)
        || cell(sor, "nvidia,active-panel", 0) != Some(phandle(panel)?)
        || cell(panel, "nvidia,lanes", 0) != Some(2)
        || cell(panel, "nvidia,is_ext_dp_panel", 0) != Some(1)
        || text(out, "nvidia,out-parent-clk") != Some("pll_d2_out0")
    {
        return Err("Icosa external DP connector or pixel parent mismatch");
    }
    let i2c = node("/i2c@7000d000")?;
    let pm = node("/i2c@7000d000/max77620@3c")?;
    if cell(pm, "reg", 0) != Some(0x3c)
        || !pm
            .compatible()
            .is_some_and(|c| c.all().any(|s| s == "maxim,max77620"))
    {
        return Err("invalid display PMIC wiring");
    }
    let regs = pm
        .children()
        .find(|n| n.name == "regulators")
        .ok_or("missing display PMIC rails")?;
    let rail = |name: &str| {
        regs.children()
            .find(|n| n.name == name && enabled(*n))
            .ok_or("missing display supply")
    };
    let core = rail("sd0")?;
    let pad = rail("ldo8")?;
    let v18 = rail("sd3")?;
    let v33 = f
        .find_phandle(cell(dc, "vdd-dp-pwr-supply", 0).ok_or("missing DP 3.3 V rail")?)
        .ok_or("invalid DP 3.3 V rail")?;
    if text(core, "regulator-name") != Some("vdd-core")
        || core.property("regulator-always-on").is_none()
        || cell(core, "regulator-min-microvolt", 0).is_none_or(|v| v > 850_000)
        || cell(core, "regulator-max-microvolt", 0).is_none_or(|v| v < 850_000)
        || cell(dc, "avdd-dp-pll-supply", 0) != Some(phandle(v18)?)
        || cell(dc, "vdd-dp-pad-supply", 0) != Some(phandle(pad)?)
        || cell(regs, "in-ldo7-8-supply", 0) != Some(phandle(rail("sd2")?)?)
        || cell(v33, "gpio", 0) != Some(phandle(pm)?)
        || cell(v33, "gpio", 1) != Some(3)
        || cell(v33, "gpio", 2) != Some(0)
        || v33.property("enable-active-high").is_none()
        || v33.property("regulator-always-on").is_none()
    {
        return Err("unexpected Icosa display regulator mapping");
    }
    for (rail, uv) in [(v33, 3_300_000), (v18, 1_800_000), (pad, 1_050_000)] {
        if !enabled(rail)
            || cell(rail, "regulator-min-microvolt", 0) != Some(uv)
            || cell(rail, "regulator-max-microvolt", 0) != Some(uv)
        {
            return Err("unexpected Icosa display supply voltage");
        }
    }
    phandle(i2c)
}

// Copyright (c) Meta Platforms, Inc. and affiliates.

// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

//! # SCX Topology
//!
//! A crate that allows schedulers to inspect and model the host's topology, in
//! service of creating scheduling domains.
//!
//! A Topology is comprised of one or more Node objects, which themselves are
//! comprised hierarchically of LLC -> Core -> Cpu objects respectively:
//!```rust,ignore
//!                                   Topology
//!                                       |
//! o--------------------------------o   ...   o----------------o---------------o
//! |         Node                   |         |         Node                   |
//! | ID      0                      |         | ID      1                      |
//! | LLCs    <id, Llc>              |         | LLCs    <id, Llc>              |
//! | Span    0x00000fffff00000fffff |         | Span    0xfffff00000fffff00000 |
//! o--------------------------------o         o--------------------------------o
//!                 \
//!                  --------------------
//!                                      \
//! o--------------------------------o   ...   o--------------------------------o
//! |             Llc                |         |             Llc                |
//! | ID     0                       |         | ID     1                       |
//! | Cores  <id, Core>              |         | Cores  <id, Core>              |
//! | Span   0x00000ffc0000000ffc00  |         | Span   0x00000003ff00000003ff  |
//! o--------------------------------o         o----------------o---------------o
//!                                                             /
//!                                        ---------------------
//!                                       /
//! o--------------------------------o   ...   o--------------------------------o
//! |              Core              |         |              Core              |
//! | ID     0                       |         | ID     9                       |
//! | Cpus   <id, Cpu>               |         | Cpus   <id, Cpu>               |
//! | Span   0x00000000010000000001  |         | Span   0x00000002000000000200  |
//! o--------------------------------o         o----------------o---------------o
//!                                                             /
//!                                        ---------------------
//!                                       /
//! o--------------------------------o   ...   o---------------------------------o
//! |              Cpu               |         |               Cpu               |
//! | ID       9                     |         | ID       49                     |
//! | online   1                     |         | online   1                      |
//! | min_freq 400000                |         | min_freq 400000                 |
//! | max_freq 5881000               |         | min_freq 5881000                |
//! o--------------------------------o         o---------------------------------o
//!```
//! Every object contains a Cpumask that spans all CPUs in that point in the
//! topological hierarchy.
//!
//! Creating Topology
//! -----------------
//!
//! Topology objects are created using the static new function:
//!
//!```  
//!     use scx_utils::Topology;
//!     let top = Topology::new().unwrap();
//!```
//!
//! Querying Topology
//! -----------------
//!
//! With a created Topology, you can query the topological hierarchy using the
//! set of accessor functions defined below. All objects in the topological
//! hierarchy are entirely read-only. If the host topology were to change (due
//! to e.g. hotplug), a new Topology object should be created.

use crate::misc::read_file_usize;
use crate::Cpumask;
use anyhow::bail;
use anyhow::Result;
use bitvec::bitvec;
use bitvec::vec::BitVec;
use glob::glob;
use sscanf::sscanf;
use std::collections::BTreeMap;
use std::path::Path;
use std::slice::Iter;
use std::sync::Arc;

#[cfg(feature = "gpu-topology")]
use crate::gpu::{create_gpus, Gpu, GpuIndex};

lazy_static::lazy_static! {
    /// The maximum possible number of CPU IDs in the system. As mentioned
    /// above, this is different than the number of possible CPUs on the
    /// system (though very seldom is). This number may differ from the
    /// number of possible CPUs on the system when e.g. there are fully
    /// disabled CPUs in the middle of the range of possible CPUs (i.e. CPUs
    /// that may not be onlined).
    pub static ref NR_CPU_IDS: usize = read_cpu_ids().unwrap().last().unwrap() + 1;

    /// The number of possible CPUs that may be active on the system. Note
    /// that this value is separate from the number of possible _CPU IDs_ in
    /// the system, as there may be gaps in what CPUs are allowed to be
    /// onlined. For example, some BIOS implementations may report spans of
    /// disabled CPUs that may not be onlined, whose IDs are lower than the
    /// IDs of other CPUs that may be onlined.
    pub static ref NR_CPUS_POSSIBLE: usize = libbpf_rs::num_possible_cpus().unwrap();
}

#[derive(Debug, Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CoreType {
    Big { turbo: bool },
    Little,
}

#[derive(Debug, Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Cpu {
    pub id: usize,
    pub min_freq: usize,
    pub max_freq: usize,
    /// Base operational frqeuency. Only available on Intel Turbo Boost
    /// CPUs. If not available, this will simply return maximum frequency.
    pub base_freq: usize,
    pub trans_lat_ns: usize,
    pub l2_id: usize,
    pub l3_id: usize,
    pub core_type: CoreType,

    /// Ancestor IDs.
    pub core_id: usize,
    pub llc_id: usize,
    pub node_id: usize,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Core {
    /// Monotonically increasing unique id
    pub id: usize,
    /// The sysfs value of core_id
    pub kernel_id: usize,
    pub cpus: BTreeMap<usize, Arc<Cpu>>,
    /// Cpumask of all CPUs in this core.
    pub span: Cpumask,
    pub core_type: CoreType,

    /// Ancestor IDs.
    pub llc_id: usize,
    pub node_id: usize,
}

#[derive(Debug, Clone)]
pub struct Llc {
    /// Monotonically increasing unique id
    pub id: usize,
    /// The kernel id of the llc
    pub kernel_id: usize,
    pub cores: BTreeMap<usize, Arc<Core>>,
    /// Cpumask of all CPUs in this llc.
    pub span: Cpumask,

    /// Ancestor IDs.
    pub node_id: usize,

    /// Skip indices to access lower level members easily.
    pub all_cpus: BTreeMap<usize, Arc<Cpu>>,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: usize,
    pub llcs: BTreeMap<usize, Arc<Llc>>,
    /// Cpumask of all CPUs in this node.
    pub span: Cpumask,

    /// Skip indices to access lower level members easily.
    pub all_cores: BTreeMap<usize, Arc<Core>>,
    pub all_cpus: BTreeMap<usize, Arc<Cpu>>,

    #[cfg(feature = "gpu-topology")]
    pub gpus: BTreeMap<GpuIndex, Gpu>,
}

#[derive(Debug)]
pub struct Topology {
    pub nodes: BTreeMap<usize, Node>,
    /// Cpumask all CPUs in the system.
    pub span: Cpumask,
    pub nr_cpus_online: usize,

    /// Skip indices to access lower level members easily.
    pub all_llcs: BTreeMap<usize, Arc<Llc>>,
    pub all_cores: BTreeMap<usize, Arc<Core>>,
    pub all_cpus: BTreeMap<usize, Arc<Cpu>>,
}

impl Topology {
    fn instantiate(span: Cpumask, mut nodes: BTreeMap<usize, Node>) -> Result<Self> {
        // Build skip indices prefixed with all_ for easy lookups. As Arc
        // objects can only be modified while there's only one reference,
        // skip indices must be built from bottom to top.
        let mut topo_llcs = BTreeMap::new();
        let mut topo_cores = BTreeMap::new();
        let mut topo_cpus = BTreeMap::new();

        for (_node_id, node) in nodes.iter_mut() {
            let mut node_cores = BTreeMap::new();
            let mut node_cpus = BTreeMap::new();

            for (&llc_id, llc) in node.llcs.iter_mut() {
                let llc_mut = Arc::get_mut(llc).unwrap();
                let mut llc_cpus = BTreeMap::new();

                for (&core_id, core) in llc_mut.cores.iter_mut() {
                    for (&cpu_id, cpu) in core.cpus.iter() {
                        if topo_cpus
                            .insert(cpu_id, cpu.clone())
                            .or(node_cpus.insert(cpu_id, cpu.clone()))
                            .or(llc_cpus.insert(cpu_id, cpu.clone()))
                            .is_some()
                        {
                            bail!("Duplicate CPU ID {}", cpu_id);
                        }
                    }

                    if topo_cores
                        .insert(core_id, core.clone())
                        .or(node_cores.insert(core_id, core.clone()))
                        .is_some()
                    {
                        bail!("Duplicate CORE ID {}", core_id);
                    }
                }

                llc_mut.all_cpus = llc_cpus;

                if topo_llcs.insert(llc_id, llc.clone()).is_some() {
                    bail!("Duplicate LLC ID {}", llc_id);
                }
            }

            node.all_cores = node_cores;
            node.all_cpus = node_cpus;
        }

        let nr_cpus_online = span.weight();
        Ok(Topology {
            nodes,
            span,
            nr_cpus_online,
            all_llcs: topo_llcs,
            all_cores: topo_cores,
            all_cpus: topo_cpus,
        })
    }

    /// Build a complete host Topology
    pub fn new() -> Result<Topology> {
        let span = cpus_online()?;
        let mut topo_ctx = TopoCtx::new();
        // If the kernel is compiled with CONFIG_NUMA, then build a topology
        // from the NUMA hierarchy in sysfs. Otherwise, just make a single
        // default node of ID 0 which contains all cores.
        let nodes = if Path::new("/sys/devices/system/node").exists() {
            create_numa_nodes(&span, &mut topo_ctx)?
        } else {
            create_default_node(&span, &mut topo_ctx, false)?
        };

        Self::instantiate(span, nodes)
    }

    pub fn with_flattened_llc_node() -> Result<Topology> {
        let span = cpus_online()?;
        let mut topo_ctx = TopoCtx::new();
        let nodes = create_default_node(&span, &mut topo_ctx, true)?;
        Self::instantiate(span, nodes)
    }

    /// Get a vec of all GPUs on the hosts.
    #[cfg(feature = "gpu-topology")]
    pub fn gpus(&self) -> BTreeMap<GpuIndex, &Gpu> {
        let mut gpus = BTreeMap::new();
        for node in &self.nodes {
            for (idx, gpu) in &node.gpus {
                gpus.insert(idx.clone(), gpu);
            }
        }
        gpus
    }

    /// Returns whether the Topology has a hybrid architecture of big and little cores.
    pub fn has_little_cores(&self) -> bool {
        self.all_cores
            .values()
            .any(|c| c.core_type == CoreType::Little)
    }

    /// Returns a BitVec of online CPUs.
    pub fn cpus_bitvec(&self) -> BitVec {
        let mut cpus = bitvec![0; *NR_CPUS_POSSIBLE];
        for id in self.all_cpus.keys() {
            cpus.set(*id, true);
        }
        cpus
    }

    /// Returns a vector that maps the index of each logical core to the sibling core.
    /// This represents the "next sibling" core within a package in systems that support SMT.
    /// The sibling core is the other logical core that shares the physical resources
    /// of the same physical core.
    ///
    /// Assuming each core holds exactly at most two cpus.
    pub fn sibling_cpus(&self) -> Vec<i32> {
        let mut sibling_cpu = vec![-1i32; *NR_CPUS_POSSIBLE];
        for core in self.all_cores.values() {
            let mut first = -1i32;
            for (cpu_id, _) in &core.cpus {
                let cpu = *cpu_id;
                if first < 0 {
                    first = cpu as i32;
                } else {
                    sibling_cpu[first as usize] = cpu as i32;
                    sibling_cpu[cpu as usize] = first;
                    break;
                }
            }
        }
        sibling_cpu
    }
}

/// Generate a topology map from a Topology object, represented as an array of arrays.
///
/// Each inner array corresponds to a core containing its associated CPU IDs. This map can
/// facilitate efficient iteration over the host's topology.
///
/// # Example
///
/// ```
/// use scx_utils::{TopologyMap, Topology};
/// let topo = Topology::new().unwrap();
/// let topo_map = TopologyMap::new(&topo).unwrap();
///
/// for (core_id, core) in topo_map.iter().enumerate() {
///     for cpu in core {
///         println!("core={} cpu={}", core_id, cpu);
///     }
/// }
/// ```
#[derive(Debug)]
pub struct TopologyMap {
    map: Vec<Vec<usize>>,
}

impl TopologyMap {
    pub fn new(topo: &Topology) -> Result<TopologyMap> {
        let mut map: Vec<Vec<usize>> = Vec::new();

        for core in topo.all_cores.values() {
            let mut cpu_ids: Vec<usize> = Vec::new();
            for cpu_id in core.span.clone().into_iter() {
                cpu_ids.push(cpu_id);
            }
            map.push(cpu_ids);
        }

        Ok(TopologyMap { map })
    }

    pub fn iter(&self) -> Iter<Vec<usize>> {
        self.map.iter()
    }

    /// Returns a vector of bit masks, each representing the mapping between
    /// physical cores and the logical cores that run on them.
    /// The index in the vector represents the physical core, and each bit in the
    /// corresponding `BitVec` represents whether a logical core belongs to that physical core.
    pub fn core_cpus_bitvec(&self) -> Vec<BitVec> {
        let mut core_cpus = Vec::<BitVec>::new();
        for (core_id, core) in self.iter().enumerate() {
            if core_cpus.len() < core_id + 1 {
                core_cpus.resize(core_id + 1, bitvec![0; *NR_CPUS_POSSIBLE]);
            }
            for cpu in core {
                core_cpus[core_id].set(*cpu, true);
            }
        }
        core_cpus
    }

    /// Returns mapping between logical core and physical core ids
    /// The index in the vector represents the logical core, and each corresponding value
    /// represents whether the physical core id of the logical core.
    pub fn cpu_core_mapping(&self) -> Vec<usize> {
        let mut cpu_core_mapping = Vec::new();
        for (core_id, core) in self.iter().enumerate() {
            for cpu in core {
                if cpu_core_mapping.len() < cpu + 1 {
                    cpu_core_mapping.resize(cpu + 1, 0);
                }
                cpu_core_mapping[*cpu] = core_id;
            }
        }
        cpu_core_mapping
    }
}

/******************************************************
 * Helper structs/functions for creating the Topology *
 ******************************************************/
/// TopoCtx is a helper struct used to build a topology.
struct TopoCtx {
    /// Mapping of NUMA node core ids
    node_core_kernel_ids: BTreeMap<(usize, usize), usize>,
    /// Mapping of NUMA node LLC ids
    node_llc_kernel_ids: BTreeMap<(usize, usize), usize>,
}

impl TopoCtx {
    fn new() -> TopoCtx {
        let core_kernel_ids = BTreeMap::new();
        let llc_kernel_ids = BTreeMap::new();
        TopoCtx {
            node_core_kernel_ids: core_kernel_ids,
            node_llc_kernel_ids: llc_kernel_ids,
        }
    }
}

fn cpus_online() -> Result<Cpumask> {
    let path = "/sys/devices/system/cpu/online";
    let online = std::fs::read_to_string(&path)?;
    let online_groups: Vec<&str> = online.split(',').collect();
    let mut mask = Cpumask::new()?;
    for group in online_groups.iter() {
        let (min, max) = match sscanf!(group.trim(), "{usize}-{usize}") {
            Ok((x, y)) => (x, y),
            Err(_) => match sscanf!(group.trim(), "{usize}") {
                Ok(x) => (x, x),
                Err(_) => {
                    bail!("Failed to parse online cpus {}", group.trim());
                }
            },
        };
        for i in min..(max + 1) {
            mask.set_cpu(i)?;
        }
    }

    Ok(mask)
}

fn create_insert_cpu(
    cpu_id: usize,
    node: &mut Node,
    online_mask: &Cpumask,
    topo_ctx: &mut TopoCtx,
    avg_cpu_freq: Option<(usize, usize)>,
    flatten_llc: bool,
) -> Result<()> {
    // CPU is offline. The Topology hierarchy is read-only, and assumes
    // that hotplug will cause the scheduler to restart. Thus, we can
    // just skip this CPU altogether.
    if !online_mask.test_cpu(cpu_id) {
        return Ok(());
    }

    let cpu_str = format!("/sys/devices/system/cpu/cpu{}", cpu_id);
    let cpu_path = Path::new(&cpu_str);

    // Physical core ID
    let top_path = cpu_path.join("topology");
    let core_kernel_id = read_file_usize(&top_path.join("core_id"))?;

    // Evaluate L2, L3 and LLC cache IDs.
    //
    // Use ID 0 if we fail to detect the cache hierarchy. This seems to happen on certain SKUs, so
    // if there's no cache information then we have no option but to assume a single unified cache
    // per node.
    let cache_path = cpu_path.join("cache");
    let l2_id = read_file_usize(&cache_path.join(format!("index{}", 2)).join("id")).unwrap_or(0);
    let l3_id = read_file_usize(&cache_path.join(format!("index{}", 3)).join("id")).unwrap_or(0);
    // Assume that LLC is always 3.
    let llc_kernel_id = if flatten_llc { 0 } else { l3_id };

    // Min and max frequencies. If the kernel is not compiled with
    // CONFIG_CPU_FREQ, just assume 0 for both frequencies.
    let freq_path = cpu_path.join("cpufreq");
    let min_freq = read_file_usize(&freq_path.join("scaling_min_freq")).unwrap_or(0);
    let max_freq = read_file_usize(&freq_path.join("scaling_max_freq")).unwrap_or(0);
    let base_freq = read_file_usize(&freq_path.join("base_frequency")).unwrap_or(max_freq);
    let trans_lat_ns = read_file_usize(&freq_path.join("cpuinfo_transition_latency")).unwrap_or(0);

    let num_llcs = topo_ctx.node_llc_kernel_ids.len();
    let llc_id = topo_ctx
        .node_llc_kernel_ids
        .entry((node.id, llc_kernel_id))
        .or_insert(num_llcs);

    let llc = node.llcs.entry(*llc_id).or_insert(Arc::new(Llc {
        id: *llc_id,
        cores: BTreeMap::new(),
        span: Cpumask::new()?,
        all_cpus: BTreeMap::new(),

        node_id: node.id,
        kernel_id: llc_kernel_id,
    }));
    let llc_mut = Arc::get_mut(llc).unwrap();

    let core_type = match avg_cpu_freq {
        Some((avg_base_freq, top_max_freq)) => {
            if max_freq == top_max_freq {
                CoreType::Big { turbo: true }
            } else if base_freq >= avg_base_freq {
                CoreType::Big { turbo: false }
            } else {
                CoreType::Little
            }
        }
        None => CoreType::Big { turbo: false },
    };

    let num_cores = topo_ctx.node_core_kernel_ids.len();
    let core_id = topo_ctx
        .node_core_kernel_ids
        .entry((node.id, core_kernel_id))
        .or_insert(num_cores);

    let core = llc_mut.cores.entry(*core_id).or_insert(Arc::new(Core {
        id: *core_id,
        cpus: BTreeMap::new(),
        span: Cpumask::new()?,
        core_type: core_type.clone(),

        llc_id: *llc_id,
        node_id: node.id,
        kernel_id: core_kernel_id,
    }));
    let core_mut = Arc::get_mut(core).unwrap();

    core_mut.cpus.insert(
        cpu_id,
        Arc::new(Cpu {
            id: cpu_id,
            min_freq,
            max_freq,
            base_freq,
            trans_lat_ns,
            l2_id,
            l3_id,
            core_type: core_type.clone(),

            core_id: *core_id,
            llc_id: *llc_id,
            node_id: node.id,
        }),
    );

    if node.span.test_cpu(cpu_id) {
        bail!("Node {} already had CPU {}", node.id, cpu_id);
    }

    // Update all of the devices' spans to include this CPU.
    core_mut.span.set_cpu(cpu_id)?;
    llc_mut.span.set_cpu(cpu_id)?;
    node.span.set_cpu(cpu_id)?;

    Ok(())
}

fn read_cpu_ids() -> Result<Vec<usize>> {
    let mut cpu_ids = vec![];
    let cpu_paths = glob("/sys/devices/system/cpu/cpu[0-9]*")?;
    for cpu_path in cpu_paths.filter_map(Result::ok) {
        let cpu_str = cpu_path.to_str().unwrap().trim();
        match sscanf!(cpu_str, "/sys/devices/system/cpu/cpu{usize}") {
            Ok(val) => cpu_ids.push(val),
            Err(_) => {
                bail!("Failed to parse cpu ID {}", cpu_str);
            }
        }
    }
    cpu_ids.sort();
    Ok(cpu_ids)
}

// Return the average base frequency across all CPUs and the highest maximum frequency.
fn avg_cpu_freq() -> Option<(usize, usize)> {
    let mut top_max_freq = 0;
    let mut avg_base_freq = 0;
    let mut nr_cpus = 0;
    let cpu_paths = glob("/sys/devices/system/cpu/cpu[0-9]*").ok()?;
    for cpu_path in cpu_paths.filter_map(Result::ok) {
        let freq_path = cpu_path.join("cpufreq");
        let max_freq = read_file_usize(&freq_path.join("scaling_max_freq")).unwrap_or(0);
        let base_freq = read_file_usize(&freq_path.join("base_frequency")).unwrap_or(max_freq);
        if base_freq > 0 {
            if max_freq > top_max_freq {
                top_max_freq = max_freq;
            }
            avg_base_freq += base_freq;
            nr_cpus += 1;
        }
    }
    if avg_base_freq == 0 {
        return None;
    }
    Some((avg_base_freq / nr_cpus, top_max_freq))
}

fn create_default_node(
    online_mask: &Cpumask,
    topo_ctx: &mut TopoCtx,
    flatten_llc: bool,
) -> Result<BTreeMap<usize, Node>> {
    let mut nodes = BTreeMap::<usize, Node>::new();

    let mut node = Node {
        id: 0,
        llcs: BTreeMap::new(),
        span: Cpumask::new()?,
        #[cfg(feature = "gpu-topology")]
        gpus: BTreeMap::new(),
        all_cores: BTreeMap::new(),
        all_cpus: BTreeMap::new(),
    };

    #[cfg(feature = "gpu-topology")]
    {
        let system_gpus = create_gpus();
        match system_gpus.get(&0) {
            Some(gpus) => {
                for gpu in gpus {
                    node.gpus.insert(gpu.index, gpu.clone());
                }
            }
            _ => {}
        };
    }

    if !Path::new("/sys/devices/system/cpu").exists() {
        bail!("/sys/devices/system/cpu sysfs node not found");
    }

    let avg_cpu_freq = avg_cpu_freq();
    let cpu_ids = read_cpu_ids()?;
    for cpu_id in cpu_ids.iter() {
        create_insert_cpu(
            *cpu_id,
            &mut node,
            &online_mask,
            topo_ctx,
            avg_cpu_freq,
            flatten_llc,
        )?;
    }

    nodes.insert(node.id, node);

    Ok(nodes)
}

fn create_numa_nodes(
    online_mask: &Cpumask,
    topo_ctx: &mut TopoCtx,
) -> Result<BTreeMap<usize, Node>> {
    let mut nodes = BTreeMap::<usize, Node>::new();

    #[cfg(feature = "gpu-topology")]
    let system_gpus = create_gpus();

    let numa_paths = glob("/sys/devices/system/node/node*")?;
    for numa_path in numa_paths.filter_map(Result::ok) {
        let numa_str = numa_path.to_str().unwrap().trim();
        let node_id = match sscanf!(numa_str, "/sys/devices/system/node/node{usize}") {
            Ok(val) => val,
            Err(_) => {
                bail!("Failed to parse NUMA node ID {}", numa_str);
            }
        };

        let mut node = Node {
            id: node_id,
            llcs: BTreeMap::new(),
            span: Cpumask::new()?,

            all_cores: BTreeMap::new(),
            all_cpus: BTreeMap::new(),

            #[cfg(feature = "gpu-topology")]
            gpus: BTreeMap::new(),
        };

        #[cfg(feature = "gpu-topology")]
        {
            match system_gpus.get(&node_id) {
                Some(gpus) => {
                    for gpu in gpus {
                        node.gpus.insert(gpu.index, gpu.clone());
                    }
                }
                _ => {}
            };
        }

        let cpu_pattern = numa_path.join("cpu[0-9]*");
        let cpu_paths = glob(cpu_pattern.to_string_lossy().as_ref())?;
        let avg_cpu_freq = avg_cpu_freq();
        for cpu_path in cpu_paths.filter_map(Result::ok) {
            let cpu_str = cpu_path.to_str().unwrap().trim();
            let cpu_id = match sscanf!(cpu_str, "/sys/devices/system/node/node{usize}/cpu{usize}") {
                Ok((_, val)) => val,
                Err(_) => {
                    bail!("Failed to parse cpu ID {}", cpu_str);
                }
            };

            create_insert_cpu(
                cpu_id,
                &mut node,
                &online_mask,
                topo_ctx,
                avg_cpu_freq,
                false,
            )?;
        }

        nodes.insert(node.id, node);
    }
    Ok(nodes)
}

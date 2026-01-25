mod entrypoints;
mod key_configs;
mod key_dirs;
mod limits;
mod markers;
mod root_scan;
mod topology;

use super::ProjectFactsResult;
use std::path::Path;

pub(super) const PROJECT_FACTS_VERSION: u32 = 1;

pub(super) fn compute_project_facts(root: &Path) -> ProjectFactsResult {
    let mut ecosystems: Vec<String> = Vec::new();
    let mut build_tools: Vec<String> = Vec::new();
    let mut ci: Vec<String> = Vec::new();
    let mut contracts: Vec<String> = Vec::new();
    let mut key_dirs: Vec<String> = Vec::new();
    let mut modules: Vec<String> = Vec::new();
    let mut entry_points: Vec<String> = Vec::new();
    let mut key_configs: Vec<String> = Vec::new();

    let Some(root_files) = root_scan::RootFiles::read(root) else {
        return ProjectFactsResult {
            version: PROJECT_FACTS_VERSION,
            ecosystems,
            build_tools,
            ci,
            contracts,
            key_dirs,
            modules,
            entry_points,
            key_configs,
        };
    };

    root_scan::apply_root_ecosystems(&root_files, &mut ecosystems);
    root_scan::apply_root_build_tools(&root_files, &mut build_tools);
    root_scan::apply_root_ci(root, &root_files, &mut ci);
    root_scan::apply_contracts(root, &mut contracts);

    key_dirs::collect_key_dirs(root, &mut key_dirs);
    topology::scan_topology(
        root,
        &mut ecosystems,
        &mut build_tools,
        &mut modules,
        &mut entry_points,
        &mut key_configs,
    );
    entrypoints::append_entrypoints(root, &mut entry_points);
    key_configs::append_key_configs(root, &mut key_configs);

    ProjectFactsResult {
        version: PROJECT_FACTS_VERSION,
        ecosystems,
        build_tools,
        ci,
        contracts,
        key_dirs,
        modules,
        entry_points,
        key_configs,
    }
}

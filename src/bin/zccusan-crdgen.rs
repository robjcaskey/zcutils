use kube::CustomResourceExt;
use std::error::Error;
use zcutils::kernel_module_artifacts::{
    ZccusanKernelModuleBundle, ZccusanKernelModuleCatalog, ZccusanKernelModuleSource,
};
use zcutils::kubernetes_storage::{
    CrossRegionReplication, MediaGrant, StorageProfile, TieringPolicy, ZcVolume,
};

fn main() -> Result<(), Box<dyn Error>> {
    for (index, crd) in [
        StorageProfile::crd(),
        MediaGrant::crd(),
        TieringPolicy::crd(),
        CrossRegionReplication::crd(),
        ZcVolume::crd(),
        ZccusanKernelModuleSource::crd(),
        ZccusanKernelModuleBundle::crd(),
        ZccusanKernelModuleCatalog::crd(),
    ]
    .into_iter()
    .enumerate()
    {
        if index != 0 {
            println!("---");
        }
        print!("{}", serde_yaml::to_string(&crd)?);
    }
    Ok(())
}

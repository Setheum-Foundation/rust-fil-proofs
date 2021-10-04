use std::fs::{self, metadata, OpenOptions};
use std::path::Path;

use anyhow::{ensure, Context, Error, Result};
use bincode::deserialize;
use blstrs::Scalar as Fr;
//use filecoin_hashers::sha256::Sha256Hasher;
use filecoin_hashers::{Domain, Hasher};
use fr32::bytes_into_fr;
use log::info;
use memmap::MmapOptions;
use storage_proofs_core::{
    cache_key::CacheKey, merkle::MerkleTreeTrait, proof::ProofScheme, util::NODE_SIZE,
};
use storage_proofs_porep::stacked::{PersistentAux, TemporaryAux, TemporaryAuxCache};
use storage_proofs_update::{
    constants::TreeRHasher, EmptySectorUpdate, PartitionProof, PrivateInputs, PublicInputs,
    PublicParams,
};

use crate::{
    constants::{DefaultPieceDomain, DefaultPieceHasher},
    pieces::verify_pieces,
    types::{Commitment, HSelect, PieceInfo, PoRepConfig, UpdateProofPartitions},
};

// FIXME: This is a debug only method
pub fn dump_elements(path: &Path) -> Result<(), Error> {
    info!("Dumping elements from {:?}", path);
    let f_data = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("could not open path={:?}", path))?;
    let data = unsafe {
        MmapOptions::new()
            .map(&f_data)
            .with_context(|| format!("could not mmap path={:?}", path))
    }?;
    let fr_size = std::mem::size_of::<Fr>() as usize;
    let end = metadata(path)?.len() as u64;
    for i in (0..end).step_by(fr_size) {
        let index = i as usize;
        let fr = bytes_into_fr(&data[index..index + fr_size])?;
        info!("[{}/{}] {:?} ", index, index + fr_size, fr);
    }

    Ok(())
}

// FIXME: This is a test only method (add to test module)
pub fn compare_elements(path1: &Path, path2: &Path) -> Result<(), Error> {
    info!("Comparing elements between {:?} and {:?}", path1, path2);
    let f_data1 = OpenOptions::new()
        .read(true)
        .open(path1)
        .with_context(|| format!("could not open path={:?}", path1))?;
    let data1 = unsafe {
        MmapOptions::new()
            .map(&f_data1)
            .with_context(|| format!("could not mmap path={:?}", path1))
    }?;
    let f_data2 = OpenOptions::new()
        .read(true)
        .open(path2)
        .with_context(|| format!("could not open path={:?}", path2))?;
    let data2 = unsafe {
        MmapOptions::new()
            .map(&f_data2)
            .with_context(|| format!("could not mmap path={:?}", path2))
    }?;
    let fr_size = std::mem::size_of::<Fr>() as usize;
    let end = metadata(path1)?.len() as u64;
    ensure!(
        metadata(path2)?.len() as u64 == end,
        "File sizes must match"
    );

    for i in (0..end).step_by(fr_size) {
        let index = i as usize;
        let fr1 = bytes_into_fr(&data1[index..index + fr_size])?;
        let fr2 = bytes_into_fr(&data2[index..index + fr_size])?;
        ensure!(fr1 == fr2, "Data mismatch when comparing elements");
    }
    info!("Match found for {:?} and {:?}", path1, path2);

    Ok(())
}

/// Encodes data into an existing replica.
#[allow(clippy::too_many_arguments)]
pub fn encode_into<'a, Tree: 'static + MerkleTreeTrait<Hasher = TreeRHasher>>(
    porep_config: PoRepConfig,
    new_replica_path: &Path,
    new_cache_path: &Path,
    sector_key_path: &Path,
    sector_key_cache_path: &Path,
    staged_data_path: &Path,
    piece_infos: &[PieceInfo],
    comm_sector_key: Commitment,
) -> Result<(Commitment, Commitment, Commitment)> {
    info!("encode_into:start");
    let mut comm_d = [0; 32];
    let mut comm_r = [0; 32];
    let mut comm_r_last = [0; 32];

    // NOTE: p_aux has comm_c and comm_r_last
    let p_aux: PersistentAux<<Tree::Hasher as Hasher>::Domain> = {
        let p_aux_path = sector_key_cache_path.join(CacheKey::PAux.to_string());
        let p_aux_bytes = fs::read(&p_aux_path)
            .with_context(|| format!("could not read file p_aux={:?}", p_aux_path))?;

        deserialize(&p_aux_bytes)
    }?;

    // Note: t_aux has labels and tree_d, tree_c, tree_r_last store configs
    let t_aux = {
        let t_aux_path = sector_key_cache_path.join(CacheKey::TAux.to_string());
        let t_aux_bytes = fs::read(&t_aux_path)
            .with_context(|| format!("could not read file t_aux={:?}", t_aux_path))?;

        let mut res: TemporaryAux<_, _> = deserialize(&t_aux_bytes)?;
        // Switch t_aux to the passed in cache_path
        res.set_cache_path(sector_key_cache_path);
        res
    };

    // Convert TemporaryAux to TemporaryAuxCache, which instantiates all
    // elements based on the configs stored in TemporaryAux.
    let t_aux_cache: TemporaryAuxCache<Tree, DefaultPieceHasher> =
        TemporaryAuxCache::new(&t_aux, sector_key_path.to_path_buf())
            .context("failed to restore contents of t_aux")?;

    let nodes_count = u64::from(porep_config.sector_size) as usize / NODE_SIZE;
    let (comm_r_domain, comm_r_last_domain, comm_d_domain) =
        EmptySectorUpdate::<'a, Tree>::encode_into(
            nodes_count,
            &t_aux_cache,
            <Tree::Hasher as Hasher>::Domain::try_from_bytes(&p_aux.comm_c.into_bytes())?,
            <Tree::Hasher as Hasher>::Domain::try_from_bytes(&comm_sector_key)?,
            &new_replica_path,
            &new_cache_path,
            &sector_key_path,
            &sector_key_cache_path,
            &staged_data_path,
            u64::from(HSelect::from(porep_config)) as usize,
        )?;

    comm_d_domain.write_bytes(&mut comm_d)?;
    comm_r_domain.write_bytes(&mut comm_r)?;
    comm_r_last_domain.write_bytes(&mut comm_r_last)?;

    ensure!(comm_d != [0; 32], "Invalid all zero commitment (comm_d)");
    ensure!(comm_r != [0; 32], "Invalid all zero commitment (comm_r)");
    ensure!(
        comm_r_last != [0; 32],
        "Invalid all zero commitment (comm_r)"
    );
    ensure!(
        verify_pieces(&comm_d, piece_infos, porep_config.into())?,
        "pieces and comm_d do not match"
    );

    info!("encode_into:finish");
    Ok((comm_r, comm_r_last, comm_d))
}

/// Reverses the encoding process and outputs the data into out_data_path.
#[allow(clippy::too_many_arguments)]
pub fn decode_from<'a, Tree: 'static + MerkleTreeTrait<Hasher = TreeRHasher>>(
    porep_config: PoRepConfig,
    out_data_path: &Path,
    replica_path: &Path,
    sector_key_path: &Path,
    sector_key_cache_path: &Path,
    comm_d_new: Commitment,
    comm_r_new: Commitment,
    comm_sector_key: Commitment, /* comm_r_last_old */
) -> Result<()> {
    info!("decode_from:start");

    // NOTE: p_aux has comm_c and comm_r_last
    let p_aux: PersistentAux<<Tree::Hasher as Hasher>::Domain> = {
        let p_aux_path = sector_key_cache_path.join(CacheKey::PAux.to_string());
        let p_aux_bytes = fs::read(&p_aux_path)
            .with_context(|| format!("could not read file p_aux={:?}", p_aux_path))?;

        deserialize(&p_aux_bytes)
    }?;

    let nodes_count = u64::from(porep_config.sector_size) as usize / NODE_SIZE;
    EmptySectorUpdate::<'a, Tree>::decode_from(
        nodes_count,
        out_data_path,
        replica_path,
        sector_key_path,
        sector_key_cache_path,
        <Tree::Hasher as Hasher>::Domain::try_from_bytes(&p_aux.comm_c.into_bytes())?,
        comm_d_new.into(),
        <Tree::Hasher as Hasher>::Domain::try_from_bytes(&comm_sector_key)?,
        u64::from(HSelect::from(porep_config)) as usize,
    )?;

    info!("decode_from:finish");
    Ok(())
}

/// Removes encoded data and outputs the sector key.
#[allow(clippy::too_many_arguments)]
pub fn remove_encoded_data<'a, Tree: 'static + MerkleTreeTrait<Hasher = TreeRHasher>>(
    porep_config: PoRepConfig,
    sector_key_path: &Path,
    sector_key_cache_path: &Path,
    replica_path: &Path,
    replica_cache_path: &Path,
    data_path: &Path,
    comm_d_new: Commitment,
    comm_r_old: Commitment,
    comm_sector_key: Commitment, /* comm_r_last_old */
) -> Result<()> {
    info!("remove_data:start");

    // NOTE: p_aux has comm_c and comm_r_last
    let p_aux: PersistentAux<<Tree::Hasher as Hasher>::Domain> = {
        let p_aux_path = replica_cache_path.join(CacheKey::PAux.to_string());
        let p_aux_bytes = fs::read(&p_aux_path)
            .with_context(|| format!("could not read file p_aux={:?}", p_aux_path))?;

        deserialize(&p_aux_bytes)
    }?;

    let nodes_count = u64::from(porep_config.sector_size) as usize / NODE_SIZE;
    EmptySectorUpdate::<'a, Tree>::remove_encoded_data(
        nodes_count,
        sector_key_path,
        sector_key_cache_path,
        replica_path,
        replica_cache_path,
        data_path,
        <Tree::Hasher as Hasher>::Domain::try_from_bytes(&p_aux.comm_c.into_bytes())?,
        comm_d_new.into(),
        <Tree::Hasher as Hasher>::Domain::try_from_bytes(&comm_sector_key)?,
        u64::from(HSelect::from(porep_config)) as usize,
    )?;

    info!("remove_data:finish");
    Ok(())
}

pub fn generate_update_proof<'a, Tree: 'static + MerkleTreeTrait<Hasher = TreeRHasher>>(
    porep_config: PoRepConfig,
    comm_r_old: Commitment,
    comm_r_new: Commitment,
    comm_d_new: Commitment,
    sector_key_path: &Path,
    sector_key_cache_path: &Path,
    replica_path: &Path,
    replica_cache_path: &Path,
) -> Result<Vec<PartitionProof<Tree>>> {
    info!("generate_update_proof:start");

    let comm_r_old_safe = <TreeRHasher as Hasher>::Domain::try_from_bytes(&comm_r_old)?;
    let comm_r_new_safe = <TreeRHasher as Hasher>::Domain::try_from_bytes(&comm_r_new)?;

    let comm_d_new_safe = DefaultPieceDomain::try_from_bytes(&comm_d_new)?;

    let public_params: storage_proofs_update::PublicParams =
        PublicParams::from_sector_size(u64::from(porep_config.sector_size));

    // NOTE: p_aux has comm_c and comm_r_last
    let p_aux_old: PersistentAux<<Tree::Hasher as Hasher>::Domain> = {
        let p_aux_path = sector_key_cache_path.join(CacheKey::PAux.to_string());
        let p_aux_bytes = fs::read(&p_aux_path)
            .with_context(|| format!("could not read file p_aux={:?}", p_aux_path))?;

        deserialize(&p_aux_bytes)
    }?;

    let public_inputs: storage_proofs_update::PublicInputs = PublicInputs {
        k: usize::from(UpdateProofPartitions::from(porep_config)),
        comm_c: p_aux_old.comm_c,
        comm_r_old: comm_r_old_safe,
        comm_d_new: comm_d_new_safe,
        comm_r_new: comm_r_new_safe,
        h: u64::from(HSelect::from(porep_config)) as usize,
    };

    // Note: t_aux has labels and tree_d, tree_c, tree_r_last store configs
    let t_aux_old = {
        let t_aux_path = sector_key_cache_path.join(CacheKey::TAux.to_string());
        let t_aux_bytes = fs::read(&t_aux_path)
            .with_context(|| format!("could not read file t_aux={:?}", t_aux_path))?;

        let res: TemporaryAux<_, _> = deserialize(&t_aux_bytes)?;
        res
    };

    // Convert TemporaryAux to TemporaryAuxCache, which instantiates all
    // elements based on the configs stored in TemporaryAux.
    let t_aux_cache_old: TemporaryAuxCache<Tree, DefaultPieceHasher> =
        TemporaryAuxCache::new(&t_aux_old, sector_key_path.to_path_buf())
            .context("failed to restore contents of t_aux_old")?;

    // Re-instantiate a t_aux with the new replica cache path, then
    // use new tree_d_config and tree_r_last_config from it.
    let mut t_aux_new = t_aux_old.clone();
    t_aux_new.set_cache_path(replica_cache_path);

    let private_inputs: PrivateInputs<Tree> = PrivateInputs {
        t_aux_old: t_aux_cache_old,
        tree_d_new_config: t_aux_new.tree_d_config,
        tree_r_new_config: t_aux_new.tree_r_last_config,
        replica_path: replica_path.to_path_buf(),
    };

    let vanilla_update_proof = EmptySectorUpdate::<'a, Tree>::prove_all_partitions(
        &public_params,
        &public_inputs,
        &private_inputs,
        usize::from(UpdateProofPartitions::from(porep_config)),
    )?;

    info!("generate_update_proof:finish");

    Ok(vanilla_update_proof)
}

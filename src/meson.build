# lago is built by cargo from the std-only crate in rust/ (no registry, no
# network: --offline --locked) and exposed here as src/lago.
sh = find_program('sh')
cargo = find_program('cargo')

lago = custom_target('lago',
  output: 'lago',
  command: [sh, files('../meson_scripts/cargo_build.sh'),
            cargo.full_path(),
            join_paths(meson.project_source_root(), 'rust', 'Cargo.toml'),
            join_paths(meson.current_build_dir(), 'cargo-target'),
            '@OUTPUT@'],
  console: true,
  build_by_default: true,
  build_always_stale: true,
  install: true,
  install_dir: get_option('bindir'))

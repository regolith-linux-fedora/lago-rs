# Regression suite: runs the built lago against a fake rofication daemon
# that the suite starts itself (python3, standard library only).
python3 = find_program('python3')

test('lago-regression', python3,
  args: ['-B', files('lago_regress.py'), lago],
  env: ['MALLOC_PERTURB_=0'],
  timeout: 60,
  is_parallel: false)

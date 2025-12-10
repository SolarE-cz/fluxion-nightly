{ pkgs ? import <nixpkgs> { config.allowUnfree = true; } }:

let
  python = pkgs.python3.withPackages (ps: [ ps.pathspec ]);

  # Use the official gemini-cli package from nixpkgs with explicit mainProgram
  gemini-cli = pkgs.gemini-cli.overrideAttrs (oldAttrs: {
    meta = oldAttrs.meta or { } // {
      mainProgram = "gemini";
    };
  });

  # List of tools to provide and fence
  tools = {
    claude = pkgs.claude-code;
    gemini = gemini-cli; # Temporarily disabled - will be re-enabled once NPM build issues are resolved
    # Add more tools here, e.g.:
    # other-tool = pkgs.some-package;
  };

  # Generate wrappers for each tool
  wrappers = builtins.mapAttrs
    (name: pkg: pkgs.writeScriptBin name ''
      #!${python}/bin/python
      import sys
      import os
      import subprocess
      import pathspec
      from pathlib import Path

      def main(args):
          cwd = os.getcwd()
          aiignore_path = Path('.aiignore')
          if not aiignore_path.exists():
              original = '${pkg}/bin/${name}'
              subprocess.check_call([original] + args)
              return

          with open(aiignore_path) as f:
              lines = f.read().splitlines()

          spec = pathspec.PathSpec.from_lines('gitignore', lines)

          hidden_dirs = []
          hidden_files = []

          for root, dirs, files in os.walk('.', topdown=True):
              rel_root = os.path.relpath(root, '.')
              for d in list(dirs):
                  rel = os.path.join(rel_root, d)
                  if spec.match_file(rel + '/'):
                      full = os.path.join(cwd, rel)
                      if os.path.exists(full) and not os.path.islink(full):
                          hidden_dirs.append(full)
                      dirs.remove(d)
              for f in files:
                  rel = os.path.join(rel_root, f)
                  if spec.match_file(rel):
                      full = os.path.join(cwd, rel)
                      if os.path.exists(full) and not os.path.islink(full):
                          hidden_files.append(full)

          # Always hide .git if it exists
          git_dir = os.path.join(cwd, '.git')
          if os.path.exists(git_dir) and git_dir not in hidden_dirs:
              hidden_dirs.append(git_dir)

          # Use bash mktemp for temporary directories and files
          empty_dir = subprocess.check_output('mktemp -d', shell=True).decode('utf-8').strip()
          blocker_dir = subprocess.check_output('mktemp -d', shell=True).decode('utf-8').strip()
          empty_file = os.path.join(blocker_dir, 'empty')
          with open(empty_file, 'w') as ef:
              pass

          # Block git binary
          try:
              git_path = subprocess.check_output('realpath $(command -v git)', shell=True).decode('utf-8').strip()
          except:
              git_path = None

          bwrap_args = [
              'bwrap',
              '--unshare-all',
              '--share-net',
              '--bind', '/', '/',
              '--dev', '/dev',
              '--proc', '/proc',
              '--chdir', cwd,
              # Add fake root privileges inside namespace (helps with Node.js quirks in some cases)
              '--uid', '0',
              '--gid', '0',
          ]

          for d in hidden_dirs:
              bwrap_args += ['--ro-bind', empty_dir, d]
          for f in hidden_files:
              bwrap_args += ['--ro-bind', empty_file, f]

          if git_path:
              bwrap_args += ['--ro-bind', empty_file, git_path]

          original = '${pkg}/bin/${name}'
          bwrap_args += [original] + args

          try:
              subprocess.check_call(bwrap_args)
          finally:
              try:
                  os.system(f'rm -rf {empty_dir}')
                  os.system(f'rm -rf {blocker_dir}')
              except:
                  pass

      if __name__ == '__main__':
          main(sys.argv[1:])
    '')
    tools;

in
pkgs.mkShell {
  buildInputs = [ pkgs.bubblewrap python ] ++ (builtins.attrValues tools);

  shellHook = ''
    export PATH="${pkgs.lib.makeBinPath (builtins.attrValues wrappers)}:$PATH"
  '';
}

(import
  (
    fetchTarball {
      url = "https://github.com/edolstra/flake-compat/archive/master.tar.gz";
      sha256 = "09m84vsz1py50giyfpx0fpc7a4i0r1xsb54dh0dpdg308lp4p188";
    }
  )
  {
    src = ./.;
  }).defaultNix

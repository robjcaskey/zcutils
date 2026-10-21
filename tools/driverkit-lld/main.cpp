// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
// Mach-O-only LLD entry point. All linking is performed by upstream LLVM code.
#include "lld/Common/Driver.h"
#include "llvm/Support/InitLLVM.h"
LLD_HAS_DRIVER(macho)
int main(int argc, const char **argv) {
  llvm::InitLLVM init(argc, argv);
  return lld::lldMain({argv, static_cast<size_t>(argc)}, llvm::outs(),
                      llvm::errs(), {{lld::Darwin, &lld::macho::link}}).retCode;
}

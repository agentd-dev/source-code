// SPDX-License-Identifier: Apache-2.0
// Imported FIRST by frames.mjs, before anything that pulls in chalk: chalk
// decides its colour level from the environment at import time, and a capture
// harness has no TTY, so without this every frame comes out monochrome — which
// is how the documentation ended up showing a colourless TUI that ships in
// colour.
process.env.FORCE_COLOR = '3';

# libx264 with the patches under patches/x264, which skip the work x264 does on
# macroblocks voxl's damage map marks unchanged (X264_MBINFO_CONSTANT): they're coded
# as skips without loading or analysing them, and only the changed ones are copied into
# x264's input frame and filtered into its lowres picture. 0001-0004 are pixelflux's
# (selkies-project/pixelflux, patches/x264); 0005 keeps the skip to where stock x264
# would take it at emergency QPs, and 0006 aligns the copy for x264's AVX-512 code.
# The stream comes out bit for bit as stock x264 codes it. They add no API, so voxl
# builds against a stock x264 too, just without the savings.
{x264}:
x264.overrideAttrs (old: {
  patches =
    (old.patches or [])
    ++ [
      ./patches/x264/0001-Skip-constant-macroblocks-without-analysing-them.patch
      ./patches/x264/0002-Copy-only-the-input-macroblocks-that-changed.patch
      ./patches/x264/0003-Filter-only-the-lowres-blocks-whose-input-changed.patch
      ./patches/x264/0004-Copy-large-changes-whole-and-changed-luma-with-the-S.patch
      ./patches/x264/0005-Skip-constant-macroblocks-only-where-the-full-path-would-at-emergency-QPs.patch
      ./patches/x264/0006-Align-changed-luma-runs-to-the-AVX-512-copy-s-64-bytes.patch
    ];
})

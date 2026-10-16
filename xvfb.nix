# Xvfb whose XTEST pointer has scroll axes, so voxl can inject smooth
# (pixel-precise) scrolling. Without it voxl falls back to wheel clicks.
{xorg}:
xorg.xorgserver.overrideAttrs (old: {
  patches = (old.patches or []) ++ [./patches/xorg-server-xtest-scroll.patch];
})

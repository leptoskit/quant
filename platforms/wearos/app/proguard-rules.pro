# R8 rules for Quant Wear OS code. leptoskit writes the rules its native library
# needs to .leptoskit/wearos/proguard-rules.pro on every build.
-keep class dev.leptoskit.quant.wearos.TileService { *; }
-keep class dev.leptoskit.quant.wearos.ComplicationDataSource { *; }

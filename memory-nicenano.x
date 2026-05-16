MEMORY
{
  /* nice!nano v2: UF2 bootloader without SoftDevice, app starts at 0x1000 (after MBR) */
  FLASH : ORIGIN = 0x00001000, LENGTH = 1020K
  RAM   : ORIGIN = 0x20000000, LENGTH = 256K
}

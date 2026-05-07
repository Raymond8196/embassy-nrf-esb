MEMORY
{
  /* nRF52840 with Open DFU Bootloader: MBR occupies first 4K */
  FLASH : ORIGIN = 0x00001000, LENGTH = 1020K
  RAM   : ORIGIN = 0x20000000, LENGTH = 256K
}

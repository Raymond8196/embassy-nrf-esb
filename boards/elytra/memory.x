MEMORY
{
  /* Elytra nRF52833 flashed bare over SWD (UF2/no-SD bootloader mass-erased).
     App boots directly from 0x0; full 512K flash / 128K RAM available. To go
     back to the UF2 bootloader, reflash utb's elytra_bootloader-1042776_nosd.hex
     and switch this back to FLASH 0x1000 / RAM 0x20000008. */
  FLASH : ORIGIN = 0x00000000, LENGTH = 512K
  RAM   : ORIGIN = 0x20000000, LENGTH = 128K
}

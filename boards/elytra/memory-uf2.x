/* Elytra nRF52833 layout for the no-SoftDevice UF2 bootloader.
   The bootloader owns the first 4 KiB and the final 48 KiB of flash.
   Build with ELYTRA_UF2=1; normal SWD builds keep using memory.x. */
MEMORY
{
  FLASH : ORIGIN = 0x00001000, LENGTH = 460K
  RAM   : ORIGIN = 0x20000008, LENGTH = 127K
}

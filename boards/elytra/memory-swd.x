/* Elytra nRF52833 layout for bare SWD flashing.
   Build without ELYTRA_UF2=1 to use the full flash and RAM. */
MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 512K
  RAM   : ORIGIN = 0x20000000, LENGTH = 128K
}

/* Sample on the P0 host during normal L2 bring-up. Columns are
 * HCR, MISR, EISR, ELRSR (hex), then the number of exits in the interval.
 */
#pragma D option quiet

vmm:vgic:exit:state
{
	@exits[arg0, arg1, arg2, arg3] = count();
}

vmm:vgic:maintenance:state
{
	printf("PPI cpu=%d host HCR=%x MISR=%x\n", cpu, arg0, arg1);
}

tick-1s
{
	printa("%x %x %x %x %@d\n", @exits);
	trunc(@exits);
}

tick-60s
{
	exit(0);
}

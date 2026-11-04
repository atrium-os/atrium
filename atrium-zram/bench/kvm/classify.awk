# Classify a dtrace `@[stack()] = count()` profile of kernel samples into
# where a swap page-out/page-in spends CPU: zram (module + the kernel zstd it
# calls), swap (swap_pager), geom, vm, other. Each stack is attributed to the
# first frame, leaf outward, that names one of those layers, so a memcpy
# inside zram counts as zram and a g_io_request under swap_pager as geom.
#   awk -f classify.awk prof.txt
function cls(f) {
	if (f ~ /atrium_zram\.ko`/ || f ~ /`(ZSTD_|HUF_|FSE_|ZSTDv|zram_)/) return "zram";
	if (f ~ /`(swap_pager|swp_|swapgeom|swapdev|swapon|blist_)/) return "swap";
	if (f ~ /`(g_|geom|biodone|bufdone|devstat|disk_)/) return "geom";
	if (f ~ /`(vm_pageout|vm_page_|vm_object|vm_fault|vm_phys|vm_reserv|pmap_|uma_|vm_map)/) return "vm";
	return "";
}
/^[ \t]*[0-9]+[ \t]*$/ {
	n = $1 + 0;
	c = "other";
	for (i = 1; i <= nf; i++) { k = cls(fr[i]); if (k != "") { c = k; break; } }
	if (idle) c = "idle";
	if (c == "other" && nf > 0) { lf = fr[1]; sub(/\+0x[0-9a-f]+$/, "", lf); oth[lf] += n; }
	tot[c] += n; all += n;
	nf = 0; idle = 0;
	next;
}
/`/ {
	fr[++nf] = $1;
	if ($1 ~ /`(sched_idletd|cpu_idle|acpi_cpu_idle|acpi_cpu_c1|cpu_idle_hlt|cpu_idle_mwait|cpu_idle_acpi)/) idle = 1;
}
END {
	busy = all - tot["idle"];
	printf "kernel samples: %d (busy %d, idle %d)\n", all, busy, tot["idle"];
	split("zram swap geom vm other", order, " ");
	for (j = 1; j <= 5; j++) {
		k = order[j];
		printf "  %-6s %9d  %5.1f%% of busy kernel\n", k, tot[k], busy ? 100 * tot[k] / busy : 0;
	}
	m = 0;
	for (f in oth) { of[++m] = f; }
	for (a = 1; a <= m; a++) for (b = a + 1; b <= m; b++)
		if (oth[of[b]] > oth[of[a]]) { t = of[a]; of[a] = of[b]; of[b] = t; }
	printf "  top \"other\" leaf functions:";
	for (a = 1; a <= m && a <= 8; a++) printf " %s %d", of[a], oth[of[a]];
	printf "\n";
	io = tot["zram"] + tot["swap"] + tot["geom"];
	if (io > 0)
		printf "§6 share: swap+geom = %.1f%% of (zram + swap + geom) [threshold 25%%]\n",
		    100 * (tot["swap"] + tot["geom"]) / io;
}

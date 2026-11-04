/*
 * vk_bench — run one compute shader through the system Vulkan driver (RADV
 * on the R9700) and time it with GPU timestamps. TEST TOOLING ONLY: it is the
 * yardstick for Atrium's own RDNA4 compiler (docs/spec/atrium-gpu-ir.md §7a),
 * run on the same SPIR-V, inputs and buffer placement as amdgpu_runner.
 *
 * Build: cc -O2 -o vk_bench vk_bench.c -lvulkan   (or -l:libvulkan.so.1)
 * Run:   ./vk_bench <bench-dir>
 *
 * bench.txt, one directive per line:
 *   spv <file>                       SPIR-V module
 *   entry <name>                     entry point
 *   groups <x> <y> <z>               workgroups
 *   repeat <n>                       submissions (inits reloaded before each)
 *   buf <binding> <size> <init|-> <name>
 *                                    storage buffer at set 0; written back to
 *                                    <name>.vk after the last run
 * Writes vk_result.txt: status, device, driver, gpu_ns (per run), gpu_ns_min,
 * gpu_ns_median.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define MAXB 16
#define CK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { fprintf(stderr, "vk_bench: %s = %d\n", #x, r_); fail(#x); } } while (0)

static FILE *res;
static const char *dir;

static void
fail(const char *what)
{
	if (res) { fprintf(res, "status=error\nerror=%s\n", what); fclose(res); }
	exit(1);
}

static void *
slurp(const char *name, size_t *n)
{
	char p[4096];
	snprintf(p, sizeof p, "%s/%s", dir, name);
	FILE *f = fopen(p, "rb");
	if (!f) { perror(p); fail("open"); }
	fseek(f, 0, SEEK_END);
	*n = (size_t)ftell(f);
	fseek(f, 0, SEEK_SET);
	void *b = malloc(*n + 4);
	if (fread(b, 1, *n, f) != *n) fail("read");
	fclose(f);
	return b;
}

static int
cmp_u64(const void *a, const void *b)
{
	uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
	return x < y ? -1 : x > y;
}

struct buf {
	uint32_t binding;
	uint64_t size;
	char init[256], name[64];
	VkBuffer b;
	VkDeviceMemory m;
	void *cpu;
	void *data;
	size_t ndata;
};

int
main(int argc, char **argv)
{
	char line[512], spv[256] = "", entry[64] = "main", p[4096];
	uint32_t gx = 1, gy = 1, gz = 1, repeat = 1, nb = 0;
	struct buf bufs[MAXB];

	if (argc != 2) { fprintf(stderr, "usage: vk_bench <dir>\n"); return 2; }
	dir = argv[1];
	snprintf(p, sizeof p, "%s/vk_result.txt", dir);
	res = fopen(p, "w");
	snprintf(p, sizeof p, "%s/bench.txt", dir);
	FILE *bt = fopen(p, "r");
	if (!res || !bt) { perror(p); return 1; }
	while (fgets(line, sizeof line, bt)) {
		struct buf *b = &bufs[nb];
		unsigned long long sz;
		if (sscanf(line, "spv %255s", spv) == 1) continue;
		if (sscanf(line, "entry %63s", entry) == 1) continue;
		if (sscanf(line, "groups %u %u %u", &gx, &gy, &gz) == 3) continue;
		if (sscanf(line, "repeat %u", &repeat) == 1) continue;
		if (nb < MAXB && sscanf(line, "buf %u %llu %255s %63s", &b->binding, &sz, b->init, b->name) == 4) {
			b->size = sz;
			nb++;
			continue;
		}
	}
	fclose(bt);
	if (!repeat) repeat = 1;

	VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
		.pApplicationName = "atrium-vk-bench", .apiVersion = (1u << 22) | (2u << 12) };
	VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
	VkInstance inst;
	CK(vkCreateInstance(&ici, NULL, &inst));
	uint32_t npd = 8;
	VkPhysicalDevice pds[8];
	CK(vkEnumeratePhysicalDevices(inst, &npd, pds));
	VkPhysicalDevice pd = VK_NULL_HANDLE;
	VkPhysicalDeviceProperties props;
	for (uint32_t i = 0; i < npd; i++) {
		vkGetPhysicalDeviceProperties(pds[i], &props);
		if (props.vendorID == 0x1002) { pd = pds[i]; break; }
	}
	if (pd == VK_NULL_HANDLE) fail("no AMD Vulkan device");
	VkPhysicalDeviceDriverProperties drv = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES };
	VkPhysicalDeviceProperties2 p2 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, .pNext = &drv };
	vkGetPhysicalDeviceProperties2(pd, &p2);
	fprintf(res, "device=%s\ndriver=%s %s\n", props.deviceName, drv.driverName, drv.driverInfo);

	uint32_t nq = 16, qf = ~0u;
	VkQueueFamilyProperties qp[16];
	vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, qp);
	/* Prefer a compute-only family: the same kind of queue amdgpu_runner uses. */
	for (uint32_t i = 0; i < nq; i++)
		if ((qp[i].queueFlags & VK_QUEUE_COMPUTE_BIT) && !(qp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT)) { qf = i; break; }
	if (qf == ~0u)
		for (uint32_t i = 0; i < nq; i++)
			if (qp[i].queueFlags & VK_QUEUE_COMPUTE_BIT) { qf = i; break; }
	if (qf == ~0u || qp[qf].timestampValidBits == 0) fail("no compute queue with timestamps");
	float prio = 1.0f;
	VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
		.queueFamilyIndex = qf, .queueCount = 1, .pQueuePriorities = &prio };
	VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
		.queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
	VkDevice dev;
	CK(vkCreateDevice(pd, &dci, NULL, &dev));
	VkQueue q;
	vkGetDeviceQueue(dev, qf, 0, &q);

	VkPhysicalDeviceMemoryProperties mp;
	vkGetPhysicalDeviceMemoryProperties(pd, &mp);
	int vram = 0;
	for (uint32_t i = 0; i < nb; i++) {
		struct buf *b = &bufs[i];
		VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = b->size,
			.usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT };
		CK(vkCreateBuffer(dev, &bci, NULL, &b->b));
		VkMemoryRequirements mr;
		vkGetBufferMemoryRequirements(dev, b->b, &mr);
		/* CPU-visible VRAM (ReBAR) like amdgpu_runner's "domain vram"; else host-visible. */
		const VkMemoryPropertyFlags want[2] = {
			VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT | VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
			VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT };
		uint32_t mt = ~0u;
		for (int w = 0; w < 2 && mt == ~0u; w++)
			for (uint32_t t = 0; t < mp.memoryTypeCount; t++)
				if ((mr.memoryTypeBits & (1u << t)) && (mp.memoryTypes[t].propertyFlags & want[w]) == want[w]) {
					mt = t;
					if (w == 0) vram++;
					break;
				}
		if (mt == ~0u) fail("no memory type");
		VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = mr.size, .memoryTypeIndex = mt };
		CK(vkAllocateMemory(dev, &mai, NULL, &b->m));
		CK(vkBindBufferMemory(dev, b->b, b->m, 0));
		CK(vkMapMemory(dev, b->m, 0, b->size, 0, &b->cpu));
		b->data = NULL;
		b->ndata = 0;
		if (strcmp(b->init, "-") != 0) b->data = slurp(b->init, &b->ndata);
		if (b->ndata > b->size) b->ndata = b->size;
	}
	fprintf(res, "vram_buffers=%d/%u\n", vram, nb);

	VkDescriptorSetLayoutBinding lb[MAXB];
	for (uint32_t i = 0; i < nb; i++)
		lb[i] = (VkDescriptorSetLayoutBinding){ .binding = bufs[i].binding,
			.descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount = 1,
			.stageFlags = VK_SHADER_STAGE_COMPUTE_BIT };
	VkDescriptorSetLayoutCreateInfo dl = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
		.bindingCount = nb, .pBindings = lb };
	VkDescriptorSetLayout dsl;
	CK(vkCreateDescriptorSetLayout(dev, &dl, NULL, &dsl));
	VkPipelineLayoutCreateInfo pl = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
		.setLayoutCount = 1, .pSetLayouts = &dsl };
	VkPipelineLayout layout;
	CK(vkCreatePipelineLayout(dev, &pl, NULL, &layout));
	size_t ncode;
	uint32_t *code = slurp(spv, &ncode);
	VkShaderModuleCreateInfo smi = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = ncode, .pCode = code };
	VkShaderModule sm;
	CK(vkCreateShaderModule(dev, &smi, NULL, &sm));
	VkComputePipelineCreateInfo cpi = { .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
		.stage = { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
			.stage = VK_SHADER_STAGE_COMPUTE_BIT, .module = sm, .pName = entry },
		.layout = layout };
	VkPipeline pipe;
	CK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpi, NULL, &pipe));

	VkDescriptorPoolSize ps = { VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, nb ? nb : 1 };
	VkDescriptorPoolCreateInfo dpi = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
		.maxSets = 1, .poolSizeCount = 1, .pPoolSizes = &ps };
	VkDescriptorPool pool;
	CK(vkCreateDescriptorPool(dev, &dpi, NULL, &pool));
	VkDescriptorSetAllocateInfo dsa = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
		.descriptorPool = pool, .descriptorSetCount = 1, .pSetLayouts = &dsl };
	VkDescriptorSet set;
	CK(vkAllocateDescriptorSets(dev, &dsa, &set));
	VkDescriptorBufferInfo bi[MAXB];
	VkWriteDescriptorSet wr[MAXB];
	for (uint32_t i = 0; i < nb; i++) {
		bi[i] = (VkDescriptorBufferInfo){ bufs[i].b, 0, bufs[i].size };
		wr[i] = (VkWriteDescriptorSet){ .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = set,
			.dstBinding = bufs[i].binding, .descriptorCount = 1,
			.descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &bi[i] };
	}
	vkUpdateDescriptorSets(dev, nb, wr, 0, NULL);

	VkQueryPoolCreateInfo qpi = { .sType = VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
		.queryType = VK_QUERY_TYPE_TIMESTAMP, .queryCount = 2 };
	VkQueryPool qpool;
	CK(vkCreateQueryPool(dev, &qpi, NULL, &qpool));
	VkCommandPoolCreateInfo cpci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = qf };
	VkCommandPool cpool;
	CK(vkCreateCommandPool(dev, &cpci, NULL, &cpool));
	VkCommandBufferAllocateInfo cba = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
		.commandPool = cpool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
	VkCommandBuffer cb;
	CK(vkAllocateCommandBuffers(dev, &cba, &cb));
	VkCommandBufferBeginInfo cbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
	CK(vkBeginCommandBuffer(cb, &cbi));
	vkCmdResetQueryPool(cb, qpool, 0, 2);
	vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
	vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, layout, 0, 1, &set, 0, NULL);
	vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, qpool, 0);
	vkCmdDispatch(cb, gx, gy, gz);
	vkCmdWriteTimestamp(cb, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, qpool, 1);
	CK(vkEndCommandBuffer(cb));
	VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
	VkFence fence;
	CK(vkCreateFence(dev, &fci, NULL, &fence));

	uint64_t *gpu = calloc(repeat, sizeof *gpu);
	for (uint32_t k = 0; k < repeat; k++) {
		for (uint32_t i = 0; i < nb; i++) {
			memset(bufs[i].cpu, 0, bufs[i].size);
			if (bufs[i].data) memcpy(bufs[i].cpu, bufs[i].data, bufs[i].ndata);
		}
		VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
		CK(vkQueueSubmit(q, 1, &si, fence));
		CK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 10000000000ull));
		CK(vkResetFences(dev, 1, &fence));
		uint64_t ts[2];
		CK(vkGetQueryPoolResults(dev, qpool, 0, 2, sizeof ts, ts, 8, VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WAIT_BIT));
		gpu[k] = (uint64_t)((double)(ts[1] - ts[0]) * props.limits.timestampPeriod);
	}
	for (uint32_t i = 0; i < nb; i++) {
		snprintf(p, sizeof p, "%s/%s.vk", dir, bufs[i].name);
		FILE *o = fopen(p, "wb");
		if (!o) fail("write output");
		fwrite(bufs[i].cpu, 1, bufs[i].size, o);
		fclose(o);
	}
	fprintf(res, "status=ok\ngpu_ns=");
	for (uint32_t k = 0; k < repeat; k++) fprintf(res, "%s%llu", k ? "," : "", (unsigned long long)gpu[k]);
	qsort(gpu, repeat, sizeof *gpu, cmp_u64);
	fprintf(res, "\ngpu_ns_min=%llu\ngpu_ns_median=%llu\n", (unsigned long long)gpu[0], (unsigned long long)gpu[repeat / 2]);
	fclose(res);
	vkDeviceWaitIdle(dev);
	return 0;
}

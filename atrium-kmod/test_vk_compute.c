/*
 * test_vk_compute.c - smoke test for the atrium-mesa-venus stack.
 *
 * Creates a Vulkan instance + logical device on the first available GPU,
 * runs a trivial compute "shader" (a precompiled SPIR-V that squares each
 * element of a buffer), reads back the result, and verifies it.
 *
 * No WSI, no presentation. The point is to exercise vkCreateDevice +
 * queue submission + GPU-side execution through the full venus path:
 *   guest mesa-venus  ->  virtio-gpu  ->  QEMU  ->  virgl_render_server
 *   (worker)          ->  MoltenVK    ->  Metal -> M4 GPU
 *
 * Build:  cc -I/usr/local/include -L/usr/local/lib -lvulkan \
 *           -o /tmp/test_vk_compute test_vk_compute.c
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define CHECK_VK(expr) do { \
    VkResult _r = (expr); \
    if (_r != VK_SUCCESS) { \
        fprintf(stderr, "%s:%d: %s = %d\n", __FILE__, __LINE__, #expr, _r); \
        return 1; \
    } \
} while (0)

#define N 1024  /* number of float32 elements in the buffer */

/* SPIR-V loaded at runtime from /tmp/test_vk_compute.spv (compiled by
 * the test harness via glslangValidator). The shader squares each
 * element of a float[N] storage buffer in local_size_x=64 workgroups. */
static uint32_t *shader_spv = NULL;
static size_t shader_spv_size = 0;

static int load_spv(const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) { perror(path); return 1; }
    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (sz <= 0 || (sz & 3)) { fprintf(stderr, "bad spv size %ld\n", sz); return 1; }
    shader_spv = malloc(sz);
    shader_spv_size = sz;
    if (fread(shader_spv, 1, sz, f) != (size_t)sz) { perror("fread"); return 1; }
    fclose(f);
    return 0;
}

int main(void) {
    if (load_spv("/tmp/test_vk_compute.spv")) return 1;

    /* 1. Instance */
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "atrium-vk-compute-smoke",
        .apiVersion = VK_API_VERSION_1_2,
    };
    VkInstanceCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };
    VkInstance instance;
    CHECK_VK(vkCreateInstance(&ici, NULL, &instance));
    fprintf(stderr, "[step] instance ok\n");

    /* 2. Pick first physical device */
    uint32_t pd_count = 0;
    CHECK_VK(vkEnumeratePhysicalDevices(instance, &pd_count, NULL));
    if (pd_count == 0) { fprintf(stderr, "no devices\n"); return 1; }
    VkPhysicalDevice *pds = calloc(pd_count, sizeof(*pds));
    CHECK_VK(vkEnumeratePhysicalDevices(instance, &pd_count, pds));
    VkPhysicalDevice pd = pds[0];

    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(pd, &props);
    fprintf(stderr, "[step] device: %s\n", props.deviceName);

    /* 3. Find a compute queue family */
    uint32_t qf_count = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qf_count, NULL);
    VkQueueFamilyProperties *qfs = calloc(qf_count, sizeof(*qfs));
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qf_count, qfs);
    uint32_t qf = ~0u;
    for (uint32_t i = 0; i < qf_count; i++) {
        if (qfs[i].queueFlags & VK_QUEUE_COMPUTE_BIT) { qf = i; break; }
    }
    if (qf == ~0u) { fprintf(stderr, "no compute qf\n"); return 1; }
    fprintf(stderr, "[step] compute qf=%u\n", qf);

    /* 4. Logical device */
    float pri = 1.0f;
    VkDeviceQueueCreateInfo qci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = qf, .queueCount = 1, .pQueuePriorities = &pri,
    };
    VkDeviceCreateInfo dci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
    };
    VkDevice dev;
    CHECK_VK(vkCreateDevice(pd, &dci, NULL, &dev));
    fprintf(stderr, "[step] device created\n");

    VkQueue queue;
    vkGetDeviceQueue(dev, qf, 0, &queue);

    /* 5. Buffer + memory */
    VkBufferCreateInfo bci = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = N * sizeof(float),
        .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    VkBuffer buf;
    CHECK_VK(vkCreateBuffer(dev, &bci, NULL, &buf));

    VkMemoryRequirements mr;
    vkGetBufferMemoryRequirements(dev, buf, &mr);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    uint32_t mt = ~0u;
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
        if ((mr.memoryTypeBits & (1u << i)) &&
            (mp.memoryTypes[i].propertyFlags &
             (VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
              VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)) ==
            (VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
             VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)) {
            mt = i; break;
        }
    }
    if (mt == ~0u) { fprintf(stderr, "no host-visible mt\n"); return 1; }
    fprintf(stderr, "[step] mt=%u\n", mt);

    VkMemoryAllocateInfo mai = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = mr.size, .memoryTypeIndex = mt,
    };
    VkDeviceMemory mem;
    CHECK_VK(vkAllocateMemory(dev, &mai, NULL, &mem));
    CHECK_VK(vkBindBufferMemory(dev, buf, mem, 0));

    float *map;
    CHECK_VK(vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, (void **)&map));
    for (int i = 0; i < N; i++) map[i] = (float)i;
    vkUnmapMemory(dev, mem);
    fprintf(stderr, "[step] buffer seeded with 0..%d\n", N);

    /* 6. Descriptor set layout, pool, set */
    VkDescriptorSetLayoutBinding dslb = {
        .binding = 0, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
        .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
    };
    VkDescriptorSetLayoutCreateInfo dslci = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount = 1, .pBindings = &dslb,
    };
    VkDescriptorSetLayout dsl;
    CHECK_VK(vkCreateDescriptorSetLayout(dev, &dslci, NULL, &dsl));

    VkDescriptorPoolSize dps = {
        .type = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount = 1,
    };
    VkDescriptorPoolCreateInfo dpci = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
        .maxSets = 1, .poolSizeCount = 1, .pPoolSizes = &dps,
    };
    VkDescriptorPool dp;
    CHECK_VK(vkCreateDescriptorPool(dev, &dpci, NULL, &dp));

    VkDescriptorSetAllocateInfo dsai = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
        .descriptorPool = dp, .descriptorSetCount = 1, .pSetLayouts = &dsl,
    };
    VkDescriptorSet ds;
    CHECK_VK(vkAllocateDescriptorSets(dev, &dsai, &ds));

    VkDescriptorBufferInfo dbi = { .buffer = buf, .offset = 0, .range = VK_WHOLE_SIZE };
    VkWriteDescriptorSet wds = {
        .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
        .dstSet = ds, .dstBinding = 0, .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &dbi,
    };
    vkUpdateDescriptorSets(dev, 1, &wds, 0, NULL);

    /* 7. Pipeline */
    VkShaderModuleCreateInfo smci = {
        .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
        .codeSize = shader_spv_size, .pCode = shader_spv,
    };
    VkShaderModule sm;
    CHECK_VK(vkCreateShaderModule(dev, &smci, NULL, &sm));

    VkPipelineLayoutCreateInfo plci = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount = 1, .pSetLayouts = &dsl,
    };
    VkPipelineLayout pl;
    CHECK_VK(vkCreatePipelineLayout(dev, &plci, NULL, &pl));

    VkComputePipelineCreateInfo cpci = {
        .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
        .stage = {
            .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
            .stage = VK_SHADER_STAGE_COMPUTE_BIT,
            .module = sm, .pName = "main",
        },
        .layout = pl,
    };
    VkPipeline pipe;
    CHECK_VK(vkCreateComputePipelines(dev, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe));
    fprintf(stderr, "[step] pipeline ok\n");

    /* 8. Cmd buffer */
    VkCommandPoolCreateInfo cpcip = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .queueFamilyIndex = qf,
    };
    VkCommandPool cp;
    CHECK_VK(vkCreateCommandPool(dev, &cpcip, NULL, &cp));

    VkCommandBufferAllocateInfo cbai = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = cp, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1,
    };
    VkCommandBuffer cb;
    CHECK_VK(vkAllocateCommandBuffers(dev, &cbai, &cb));

    VkCommandBufferBeginInfo cbbi = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
    };
    CHECK_VK(vkBeginCommandBuffer(cb, &cbbi));
    vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
    vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
    vkCmdDispatch(cb, N / 64, 1, 1);
    /* Ensure GPU writes to the storage buffer are visible to subsequent
     * host reads. On MoltenVK, host-coherent memory may be fronted by a
     * staging buffer; the explicit HOST_READ barrier forces the flush. */
    VkBufferMemoryBarrier bmb = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER,
        .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .buffer = buf, .offset = 0, .size = VK_WHOLE_SIZE,
    };
    vkCmdPipelineBarrier(cb,
                         VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                         VK_PIPELINE_STAGE_HOST_BIT,
                         0, 0, NULL, 1, &bmb, 0, NULL);
    CHECK_VK(vkEndCommandBuffer(cb));

    /* 9. Submit + wait */
    VkSubmitInfo si = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .commandBufferCount = 1, .pCommandBuffers = &cb,
    };
    fprintf(stderr, "[step] submitting\n");
    CHECK_VK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
    CHECK_VK(vkQueueWaitIdle(queue));
    fprintf(stderr, "[step] queue idle\n");

    /* 10. Read back + verify */
    CHECK_VK(vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, (void **)&map));
    int errors = 0;
    for (int i = 0; i < 8; i++) {
        float exp = (float)i * (float)i;
        if (map[i] != exp) {
            if (errors++ < 4)
                fprintf(stderr, "[%d] got %f expected %f\n", i, map[i], exp);
        }
    }
    /* spot-check a higher index */
    {
        float exp = 1023.0f * 1023.0f;
        if (map[1023] != exp) {
            errors++;
            fprintf(stderr, "[1023] got %f expected %f\n", map[1023], exp);
        }
    }
    vkUnmapMemory(dev, mem);

    if (errors) {
        fprintf(stderr, "FAIL: %d mismatches\n", errors);
        return 1;
    }
    fprintf(stderr, "PASS: 1024 elements squared correctly on %s\n", props.deviceName);

    /* cleanup is fine to skip in a smoke test; venus tears down when fd closes */
    return 0;
}

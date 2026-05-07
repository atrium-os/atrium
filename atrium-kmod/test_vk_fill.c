/*
 * test_vk_fill.c - minimum venus stack smoke. Just vkCmdFillBuffer.
 *
 * No compute pipeline, no descriptor sets. Just allocate a buffer,
 * fill it via the GPU's blit/transfer pipeline, and read back. If
 * GPU writes are reaching our shared memory at all, fillBuffer is
 * the simplest way to prove it.
 *
 * Build: cc -I/usr/local/include -L/usr/local/lib -o /tmp/test_vk_fill \
 *         test_vk_fill.c -lvulkan
 */

#include <stdio.h>
#include <stdlib.h>
#include <vulkan/vulkan.h>

#define CHECK(expr) do { VkResult _r = (expr); \
    if (_r != VK_SUCCESS) { fprintf(stderr, "%s:%d: %s = %d\n", __FILE__, __LINE__, #expr, _r); return 1; } \
} while (0)

#define N 1024

int main(void) {
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "test_vk_fill",
        .apiVersion = VK_API_VERSION_1_2,
    };
    VkInstanceCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };
    VkInstance instance;
    CHECK(vkCreateInstance(&ici, NULL, &instance));

    uint32_t pd_count = 1;
    VkPhysicalDevice pd;
    CHECK(vkEnumeratePhysicalDevices(instance, &pd_count, &pd));

    uint32_t qf_count = 1;
    VkQueueFamilyProperties qf_props;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &qf_count, &qf_props);
    /* Any queue supports VK_QUEUE_TRANSFER_BIT (compute or graphics include it).
       Just use family 0. */

    float pri = 1.0f;
    VkDeviceQueueCreateInfo qci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = 0, .queueCount = 1, .pQueuePriorities = &pri,
    };
    VkDeviceCreateInfo dci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
    };
    VkDevice dev;
    CHECK(vkCreateDevice(pd, &dci, NULL, &dev));
    VkQueue queue;
    vkGetDeviceQueue(dev, 0, 0, &queue);

    VkBufferCreateInfo bci = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = N * sizeof(uint32_t),
        .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    VkBuffer buf;
    CHECK(vkCreateBuffer(dev, &bci, NULL, &buf));

    VkMemoryRequirements mr;
    vkGetBufferMemoryRequirements(dev, buf, &mr);

    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    uint32_t mt = ~0u;
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
        if ((mr.memoryTypeBits & (1u << i)) &&
            (mp.memoryTypes[i].propertyFlags &
             (VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)) ==
            (VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)) {
            mt = i; break;
        }
    }
    if (mt == ~0u) { fprintf(stderr, "no host-visible mt\n"); return 1; }

    VkMemoryAllocateInfo mai = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = mr.size, .memoryTypeIndex = mt,
    };
    VkDeviceMemory mem;
    CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
    CHECK(vkBindBufferMemory(dev, buf, mem, 0));

    uint32_t *map;
    CHECK(vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, (void **)&map));
    for (int i = 0; i < N; i++) map[i] = 0xAAAAAAAAu;
    vkUnmapMemory(dev, mem);

    VkCommandPoolCreateInfo cpci = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .queueFamilyIndex = 0,
    };
    VkCommandPool cp;
    CHECK(vkCreateCommandPool(dev, &cpci, NULL, &cp));

    VkCommandBufferAllocateInfo cbai = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = cp, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1,
    };
    VkCommandBuffer cb;
    CHECK(vkAllocateCommandBuffers(dev, &cbai, &cb));

    VkCommandBufferBeginInfo cbbi = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
    };
    CHECK(vkBeginCommandBuffer(cb, &cbbi));
    /* Fill the entire buffer with sentinel 0xCAFEBABE */
    vkCmdFillBuffer(cb, buf, 0, VK_WHOLE_SIZE, 0xCAFEBABEu);
    VkBufferMemoryBarrier bmb = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER,
        .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .buffer = buf, .offset = 0, .size = VK_WHOLE_SIZE,
    };
    vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                         0, 0, NULL, 1, &bmb, 0, NULL);
    CHECK(vkEndCommandBuffer(cb));

    VkSubmitInfo si = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .commandBufferCount = 1, .pCommandBuffers = &cb,
    };
    fprintf(stderr, "submitting fillBuffer(0xCAFEBABE)\n");
    CHECK(vkQueueSubmit(queue, 1, &si, VK_NULL_HANDLE));
    CHECK(vkQueueWaitIdle(queue));
    fprintf(stderr, "queue idle\n");

    CHECK(vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, (void **)&map));
    int ok_count = 0;
    for (int i = 0; i < 8; i++) {
        fprintf(stderr, "buf[%d] = 0x%08x\n", i, map[i]);
        if (map[i] == 0xCAFEBABEu) ok_count++;
    }
    if (map[N-1] == 0xCAFEBABEu) ok_count++;
    fprintf(stderr, "%s: %d/9 slots show 0xCAFEBABE\n",
            ok_count == 9 ? "PASS" : "FAIL", ok_count);
    vkUnmapMemory(dev, mem);

    return ok_count == 9 ? 0 : 1;
}

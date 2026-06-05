package main

/*
#include <stdint.h>
#include <stdlib.h>

typedef void (*MessageCallback)(char* message, void* user_data);

static inline void call_message_callback(MessageCallback cb, char* message, void* user_data) {
    cb(message, user_data);
}
*/
import "C"
import (
	"sync"
	"unsafe"
)

type client struct {
	dbPath string
}

var (
	mu       sync.Mutex
	clients  = map[uint64]*client{}
	nextID   uint64 = 1
	msgCb    C.MessageCallback
	msgCbCtx unsafe.Pointer
)

//export C_NewClient
func C_NewClient(dbPath *C.char) C.uint64_t {
	mu.Lock()
	defer mu.Unlock()

	id := nextID
	nextID++
	clients[id] = &client{dbPath: C.GoString(dbPath)}
	return C.uint64_t(id)
}

//export C_Connect
func C_Connect(clientID C.uint64_t) C.uint8_t {
	mu.Lock()
	defer mu.Unlock()

	if _, ok := clients[uint64(clientID)]; !ok {
		return 0
	}
	return 1
}

//export C_SetMessageCallback
func C_SetMessageCallback(cb C.MessageCallback, userData unsafe.Pointer) {
	mu.Lock()
	defer mu.Unlock()

	msgCb = cb
	msgCbCtx = userData
}

//export C_FireSyntheticMessage
func C_FireSyntheticMessage(message *C.char) C.uint8_t {
	mu.Lock()
	cb := msgCb
	ctx := msgCbCtx
	mu.Unlock()

	if cb == nil {
		return 0
	}

	messageCopy := C.GoString(message)
	go func() {
		copy := C.CString(messageCopy)
		defer C.free(unsafe.Pointer(copy))
		C.call_message_callback(cb, copy, ctx)
	}()

	return 1
}

//export C_Disconnect
func C_Disconnect(clientID C.uint64_t) {
	mu.Lock()
	defer mu.Unlock()

	delete(clients, uint64(clientID))
}

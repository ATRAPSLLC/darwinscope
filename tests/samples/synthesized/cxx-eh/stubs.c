/* The C++ runtime the image imports, and the binder libSystem would provide;
   nothing here runs. */
void *__cxa_allocate_exception(unsigned long n) { return 0; }
void __cxa_throw(void *a, void *b, void (*c)(void *)) {}
void *__cxa_begin_catch(void *e) { return e; }
void __cxa_end_catch(void) {}
void __cxa_rethrow(void) {}
void _Unwind_Resume(void *e) {}
int __gxx_personality_v0(void) { return 0; }
void __clang_call_terminate(void *e) {}
void *_ZTIi[2];
void *_ZTVN10__cxxabiv117__class_type_infoE[4];
void *_ZTVN10__cxxabiv120__si_class_type_infoE[4];
void _ZSt9terminatev(void) {}
void dyld_stub_binder(void) __asm__("dyld_stub_binder");
void dyld_stub_binder(void) {}
